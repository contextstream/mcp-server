import { describe, it, expect } from "vitest";
import { HotPathStore } from "./hot-paths.js";

describe("HotPathStore", () => {
  it("builds hints from search/activity signals", () => {
    const store = new HotPathStore();
    const scope = { workspace_id: "ws-a", project_id: "proj-a" };
    store.recordPaths(scope, ["src/search.ts", "src/tools.ts"], "search_result");
    store.recordPaths(scope, ["src/tools.ts"], "activity_read");

    const hint = store.buildHint({
      ...scope,
      query: "where is search implemented",
      active_paths: ["src/tools.ts"],
      limit: 5,
    });

    expect(hint).toBeDefined();
    expect(hint?.entries.length).toBeGreaterThan(0);
    expect(hint?.entries[0].path).toBe("src/tools.ts");
    expect(hint?.confidence).toBeGreaterThan(0);
  });
});
