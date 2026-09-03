# Changelog

## Unreleased

- Wave 3b parity: `memory(action="decisions")` requests the typed
  `decisions.v1` envelope (query, category, sort, status, since, offset) and
  renders `[DECISIONS]` / `[DECISION]` lines with status, freshness, category,
  and id, plus `[PARTIAL]` lines for every degraded source (including servers
  that still return the legacy array). New `memory(action="create_decision")`
  and `memory(action="decision_action")`; `session(action="capture",
  event_type="decision")` routes to the typed create when rationale,
  alternatives, scope, or confidence are present; `supersede_node` accepts
  lookup text and returns a `[CANDIDATES]` list when ambiguous;
  `decision_trace` renders `[DECISION_TRACE]` with the server answer.
- Lessons: `capture_lesson`, `get_lessons`, `update_lesson`, `delete_lesson`,
  and the new `supersede_lesson` go to `/lessons` first and fall back to the
  events path only on 404 (stated with a `[PARTIAL]` line). `context()` and
  `session(action="ground")` render `[LESSONS_WARNING]` through one renderer
  (stored severity, relevance shown separately). Suggested-rule actions
  render typed `[SUGGESTED_RULES]` lines with `source_lesson_ids` and the
  native guidance snippet.
- Coordination: `context()` fetches the coordination inbox (skipped on the
  fast route) and `context()`/`init()` check in when a session id is
  present; `[COORDINATION]` lines prefix other-project notices with
  `[other project]`, add a `… N more` trailer, and are never auto-acked;
  `share` validates `kind` client-side.
- Hygiene: `[RULES_NOTICE]` now names the real refresh path
  (`contextstream-mcp update`, previewed by `help(action="editor_rules")`)
  instead of a phantom `generate_rules()` tool; the phantom `graph_decisions`
  entry left the light toolset; `memory_decisions` is reachable in
  consolidated mode; grounding hits with `superseded_by` are marked
  `stale=true, stale_reason="superseded"`; `HarnessId::ContextCode`
  (`contextcode`, `csc`, …) gets capability-aware teaching.

- Added the `feed` tool for ContextStream Context Feeds (list, ensure, get,
  update, archive, items, post, follow, unfollow, read, share, unshare,
  feedback, curate, runs, sources, ground) with a `feeds` bundle, typed
  client methods, and `[FEED]` lines plus structured `feed_items` in
  `session(action="ground")`. Feeds require a ContextStream deployment with
  `CONTEXTSTREAM_FEEDS_API_ENABLED`; the tool reports when the API is absent.

## 1.0.0

- Replaced the legacy TypeScript implementation with the canonical Rust MCP
  server while preserving public repository history.
- Added the MongoDB-free remote acceleration build.
- Added a dependency-free npm compatibility launcher with exact-version
  downloads, SHA-256 verification, atomic caching, offline reuse, and the
  `mcp-server`, `contextstream-mcp`, and `contextstream-hook` aliases.
- Added dual Streamable HTTP and npm stdio MCP Registry metadata.
- Minimized VCS capture to opaque checkout IDs, credential-free remotes,
  bounded/redacted subjects, and aggregate metadata; author identity and raw
  paths are not transmitted.
- Moved build, security, attestation, and release authority to this repository.
