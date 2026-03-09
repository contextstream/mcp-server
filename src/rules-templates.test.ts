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
    expect(copilotFiles.map((file) => file.filename)).toContain(
      ".github/copilot-instructions.md"
    );
    expect(copilotFiles.map((file) => file.filename)).toContain(
      ".github/skills/contextstream-workflow/SKILL.md"
    );
    expect(
      copilotFiles.find((file) => file.filename.endsWith("SKILL.md"))?.content
    ).toContain("name: contextstream-workflow");
  });
});
