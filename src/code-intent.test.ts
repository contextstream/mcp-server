import { describe, it, expect } from "vitest";
import { queryHasCodeIntent } from "./tools.js";

describe("queryHasCodeIntent", () => {
  it("recognizes lone identifier-shaped tokens", () => {
    expect(queryHasCodeIntent("search_first_redirect_decision")).toBe(true);
    expect(queryHasCodeIntent("resolveWriteScope")).toBe(true);
    expect(queryHasCodeIntent("SessionManager")).toBe(true);
    expect(queryHasCodeIntent("scope::resolve_write_scope")).toBe(true);
    expect(queryHasCodeIntent("client.getProject")).toBe(true);
  });

  it("leaves prose and plain memory words unaffected", () => {
    expect(queryHasCodeIntent("decisions")).toBe(false);
    expect(queryHasCodeIntent("hexagon logo")).toBe(false);
    expect(queryHasCodeIntent("how does auth work here")).toBe(false);
    expect(queryHasCodeIntent("")).toBe(false);
  });

  it("recognizes identifiers embedded in multi-word queries", () => {
    expect(queryHasCodeIntent("where is resolveWriteScope used")).toBe(true);
    expect(queryHasCodeIntent("find delete_all handler")).toBe(true);
  });
});
