# Changelog

## 0.4.72

**Feature parity pass 2: search quality, smart context surfacing, full VCS API, IndexKeeper.**

### Search Quality

- **Artifact path filtering** — Post-API filtering removes results from `.next/`, `node_modules/`, `dist/`, `build/`, `target/`, `coverage/`, `archives-ignore/`, and source map files. Bypassed for `pattern`/`exhaustive` modes and queries targeting artifacts.
- **Mode escalation** — When primary mode returns 0 results, progressively retries broader modes (semantic -> hybrid -> keyword, etc.).
- **Scope-invalid candidate skipping** — Search fallback loop now skips candidates returning `project_access_denied` or `scope_invalid`, trying the next candidate instead of returning empty.
- **Path canonicalization** — Strips internal storage prefixes (`contextstream-ai-brain-export/`, `web/users/`, `.claude/worktrees/`) and deduplicates results by canonical path.
- **Parallel ripgrep pre-fetch** — For identifier queries, spawns ripgrep in parallel with the API call (not just zero-result fallback). Merges deduplicated results.
- **Symbol anchor reranking** — Extracts symbol-like tokens from queries and promotes results matching those tokens, demoting artifact/doc paths.
- **Concise tool text** — New `CONTEXTSTREAM_CONCISE_TOOL_TEXT` env var (default: on). Suppresses mode selection notes and hot-path details when results are present.
- **Stale project_id messaging** — Invalid project IDs now return "Do NOT pass this project_id again" to prevent AI from repeating bad IDs.

### Smart Context Surfacing

- **Typed context items** — New `SmartContextItem`, `ContextItemKind`, `Precedence`, `ContextManifest` types with wire code mapping (W/P->Rule, L->Lesson, D->Decision, VC->Vcs, PR->Preference, SK->Skill, TN->TranscriptSnapshot).
- **Three-tier context path** — Fast mode (~20-50ms cached response), warm cache (30s TTL for turns 2+), and full smart call. Reduces latency on subsequent turns.
- **Typed item rendering** — When API returns `items[]`, renders by kind with precedence ordering. Formatting helpers for preferences, lessons, VCS, skills, and transcript snapshots. Compact mode uses terse `[PREF]`, `[LESSON]`, `[VCS]` tags.
- **Proactive VCS context** — On early turns (<= 3), parallel fetch of open PRs, issues from linked VCS repos. Deduplicates against server-provided typed VCS items.
- **Proactive recent changes** — On turns <= 2, parallel `git log --oneline -5` appended as `[RECENT_CHANGES]` block.

### VCS API Integration

- **Full 49-action VCS proxy** — Expanded from 6 local git actions to full API coverage: repos (list/get/sync), pull requests (list/get/diff/comments/commits/checks/summary/review/comment/merge), issues (list/get/create/update/comment), commits (list/get/diff/compare), branches/tags, tree/blob, search, activity, notifications, links, automations, webhooks.
- **VCS client methods** — `vcsApiRequest()`, `getVcsRepos()`, `getVcsResource()` added to client.

### Project & Index Maintenance

- **HTTP transport ingest delegation** — When `ingest_local` path doesn't exist locally, delegates to API via `POST /projects/{id}/files/ingest-from-path`.
- **IndexKeeper** — Background maintenance service: incremental check (10s), aging refresh (5min, index > 4h, 20k file cap), stale re-ingest (60s, triggered post-search).
- **Batch retry** — Failed ingest batches are retried once before continuing to the next batch.
- **Deterministic file walk** — All `walkDir` functions now sort directory entries by name for consistent hash manifests.

## 0.4.71

**Feature parity with Rust MCP v0.2.22, 8 GitHub issue fixes, and search enrichment.**

### Critical Fixes

- **SDK version pin (Issue #36)** — Pin `@modelcontextprotocol/sdk` to `>=1.25.1 <1.28.0`. Versions 1.28.0+ break all ContextStream installs with a Zod schema error. New installs now resolve to a working SDK version.

- **list_events filtering (Issue #34)** — Consolidated `memory(action="list_events")` now passes `tags` and `event_type` filter parameters to the API and applies client-side post-filtering using `extractEffectiveEventType()` and `extractEventTags()`. Previously all filters were silently dropped.

- **Event type preservation (Issue #35)** — Capture flows now store the original event type (`lesson`, `insight`, `preference`, etc.) instead of normalizing everything to `manual_note`. The `extractEffectiveEventType()` helper now prioritizes `metadata.original_type` when the top-level type is `manual_note`.

### Multi-Field Detection Fixes (Issue #38)

- **graph(decisions)** — Falls back to `memory(decisions)` when graph query returns empty, ensuring decisions captured via MCP are always retrievable.
- **session(summary)** — Enriches zero-count summaries with client-side event counting using `isDecisionResult()` and `isLessonResult()`.
- **session(decision_trace)** — Adds timeout handling with keyword-based fallback using `isDecisionResult()` on recent events.
- **session(recall)** — Fixes misleading "No memories found" hint when `memory_results.data.results` contains actual data.

### Search Improvements

- **Embedding timeout fallback (Issue #37)** — When semantic/auto search fails with "Embedding timed out", automatically retries with keyword mode instead of returning an error.
- **Local ripgrep enrichment** — Zero-result searches now fall back to local `rg` (ripgrep) subprocess search, providing results even when the API returns nothing.
- **Code identifier routing** — Multi-word queries containing camelCase or snake_case tokens now route to hybrid mode instead of pure semantic for better code search.
- **Refactor mode fallback** — `search.refactor` gracefully falls back to keyword search if the `/search/refactor` endpoint returns 404.

### New Tools

- **VCS tool** — New `vcs` consolidated tool for git operations: `status`, `diff`, `log`, `blame`, `branches`, `stash_list`. Read-only git subprocess calls scoped to the project directory.

### Audit Fixes (ported from Rust MCP v0.2.22)

- `project.recent_changes` falls back to `cwd` when no folder path is available.
- `graph.contradictions` now accepts optional `node_id` (returns hint instead of error when omitted).
- Integration client paths fixed: `githubSummary`, `slackSummary`, and `integrationsStatus` now use workspace-scoped API routes.
- Plan ghost titles sanitized: "(No assistant output found...)" replaced with "Untitled plan".
- Transcript missing titles: generates `{type} transcript — {date}` fallback.
- Lessons deduplicated by normalized title in `get_lessons`.

```bash
npm install -g @contextstream/mcp-server@0.4.71
```

---

## 0.4.70

**Kilo Code editor support and MCP env wizard improvements.**

- Added Kilo Code (`kilo.jsonc`) MCP config generation in setup wizard.
- Aligned VS Code and hosted MCP default paths.
- Default hosted MCP to fast context mode.

---

## 0.4.69

**Global workspace-only fallback, project-scope remediation, and hot-path reliability.**

- Global workspace fallback when project scope resolution fails.
- Project-scope remediation for stale or deleted project mappings.
- Hot-path store reliability improvements.

---

## 0.4.68

**Patch release — version bump and dependency updates.**

- Bump `hono` from 4.12.5 to 4.12.8.
- Bump `@hono/node-server` from 1.19.9 to 1.19.11.
- Bump `ajv` from 6.12.6 to 6.14.0.

---

## 0.4.67

**Streamlined VS Code and Copilot onboarding.**

- Simplified README onboarding instructions.
- Added marketplace environment placeholders.

---

## 0.4.66

**Query tools fix, tag-based filtering, and Skills tool (Rust parity).**

### Fixes

- **Event type fallback (Issue #31)** — Query tools now use multi-field detection (`isLessonResult`, `isDecisionResult`, `extractEffectiveEventType`) to handle API event type normalization.
- **Tag filtering (Issue #32)** — Client-side tag post-filtering for `list_events` when API-side filtering is incomplete.

### New

- **Skills tool** — Full skill management: `list`, `get`, `create`, `update`, `run`, `delete`, `import`, `export`, `share`. Ported from Rust MCP for parity.
- **Lesson truncation limits** — Increased preview truncation from 120 to 1000 characters.

```bash
npm install -g @contextstream/mcp-server@0.4.66
```

---

## 0.4.65

**Tag propagation fix, dependency bumps, and opencode config support.**

- **Tag propagation fix (PR #18)** — Tags now correctly propagate through capture and query flows.
- **opencode MCP config support (PR #26)** — Added config generation for the opencode editor.
- Bump `picomatch` from 4.0.3 to 4.0.4.
- Bump `flatted` from 3.3.3 to 3.4.2.

```bash
npm install -g @contextstream/mcp-server@0.4.65
```

---

## 0.4.64

**Decision query fixes, Dart indexing, Copilot rules generation, and todo state compatibility.**

### Fixes and Improvements

- **Decision capture fix** — `session(action="capture", event_type="decision")` now preserves the stored `decision` event type so `memory(action="decisions")` and `session(action="decision_trace")` can find captured decisions correctly.

- **Dart indexing support** — Added `.dart` to the indexed source extensions and language detection so Dart and Flutter projects are included in search and indexing flows.

- **GitHub Copilot rules support** — `generate_rules` and `generate_editor_rules` now support `copilot`, generating `.github/copilot-instructions.md` and `.github/skills/contextstream-workflow/SKILL.md`.

- **Todo completion compatibility** — Todo completion/update flows now map `completed`, `todo_status`, and the status alias consistently so dashboard checkbox actions and MCP todo mutations stay in sync.

```bash
npm install -g @contextstream/mcp-server@latest
```

---

## 0.4.45

**Content management, team features, and real-time indexing.**

### Content Management

New lightweight content tools for quick capture without heavyweight plans:

- **Todos** — Simple task tracking via `memory` tool. Actions: `create_todo`, `list_todos`, `get_todo`, `update_todo`, `complete_todo`, `delete_todo`. Supports priority levels and due dates.

- **Diagrams** — Mermaid diagram storage via `memory` tool. Actions: `create_diagram`, `list_diagrams`, `get_diagram`, `update_diagram`, `delete_diagram`. Supports flowchart, sequence, class, ER, gantt, mindmap, and pie charts.

- **Docs** — Markdown documents via `memory` tool. Actions: `create_doc`, `list_docs`, `get_doc`, `update_doc`, `delete_doc`, `create_roadmap`. Includes roadmap templates with milestones.

### Team Features

New team-wide tools:

- `help(action="team_status")` — Team overview with seats and members
- `session(action="team_decisions")` — Aggregate decisions across team workspaces
- `session(action="team_lessons")` — Aggregate lessons across team workspaces
- `workspace(action="team_members")` — List team members with access
- `project(action="team_projects")` — List all team projects
- `integration(action="team_activity")` — Aggregated activity from Notion, Slack, GitHub
- `search(mode="team")` — Cross-project search across team workspaces

### Other Improvements

- **Real-time file indexing** — Files indexed automatically during AI sessions via PostToolUse hook
- **All hooks converted for better compatibility** — Hooks now use Node.js instead of Python
- **Renamed tools** — `session_init` → `init`, `context_smart` → `context`
- **Cleaner output** — Reduced verbosity for rules and search reminders

```bash
npm install -g @contextstream/mcp-server@latest
```

---

## 0.4.44

**Media tool for AI-powered video editing.**

- **Media Tool** — Index, search, and retrieve clips from video/audio with semantic understanding. Actions: `index`, `status`, `search`, `get_clip`, `list`, `delete`. Designed for Remotion and FFmpeg workflows.

- **Semantic Intent** — `context_smart` now returns intent classification for Pro+ users.

```bash
npm install -g @contextstream/mcp-server@latest
```

---

## 0.4.43

**Enhanced Context warnings and Notion reliability improvements.**

This release adds support for server-side Enhanced Context warnings and fixes a common Notion integration issue.

### What's New

- **Enhanced Context Warnings** — `context_smart` now surfaces server-side warnings for lessons, risky actions, and breaking changes. When the API detects relevant lessons or risky operations (like migrations or deployments), warnings are automatically included in the response and displayed with ⚠️ prefixes. This is part of the new Enhanced Context feature for Pro+ users.

- **Notion Database ID Validation** — Fixed a common issue where AI agents would use stale database IDs from memory, causing 404 errors. The `notion_create_page` tool now clearly warns that you must call `list_databases` first to get valid IDs. This prevents the frustrating "database not found" errors.

### Upgrading

```bash
npm install -g @contextstream/mcp-server@latest
```

---

## 0.4.42

**Streamlined setup wizard and cleaner output.**

The setup experience is now simpler with fewer prompts, and the server produces much cleaner terminal output.

### What's New

- **Simplified Setup Wizard** — Removed the rules detail level prompt (now always uses enhanced rules). Removed Windsurf editor support. MCP config now defaults to project-level instead of global+project.

- **Version Check on Setup** — When running `npx -y @contextstream/mcp-server setup`, you'll now see a warning if you're running an outdated cached version, with clear instructions to get the latest.

- **Cleaner Server Output** — New `CONTEXTSTREAM_LOG_LEVEL` environment variable controls verbosity:
  - `quiet` — Minimal output, errors only
  - `normal` (default) — Clean startup message
  - `verbose` — Full debug output (legacy behavior)

- **Reliable Publishing** — Added `prepublishOnly` hook to ensure builds happen before npm publish.

### Upgrading

```bash
npm install -g @contextstream/mcp-server@latest
```

Or re-run setup:

```bash
npx -y @contextstream/mcp-server@latest setup
```

---

## 0.4.41

**Bug fix release.**

Fixed an issue where npm publish wasn't including the latest build artifacts.

---

## 0.4.40

**Setup wizard improvements.**

- Added version check at setup start to warn about outdated cached versions
- Changed upgrade command to use `@latest` for reliable updates

---

## 0.4.35

**Stronger enforcement for ContextStream-first search.**

The hooks now block *all* Grep/Search operations, not just codebase-wide searches. If your AI tries to grep within a specific file, it gets redirected to use `Read()` instead.

### What's New

- **Smart index detection** — Hooks now only block local tools for projects that are actually indexed. If a project hasn't been indexed yet, local tools work normally so you're not stuck. Once you run `ingest_local`, hooks automatically start enforcing ContextStream-first behavior.

- **More aggressive hooks** — Previously, Grep/Search on specific file paths was allowed through. Now all Grep/Search operations are blocked with clear guidance: use `Read()` for viewing specific files, or ContextStream search for codebase queries.

### Upgrading

```bash
npm update @contextstream/mcp-server
npx -y @contextstream/mcp-server setup  # Re-run to update hooks
```

---

## 0.4.34

**Your AI assistant just got better at following instructions.**

This release focuses on making sure your AI actually uses ContextStream when it should—no more watching it grep through files when a single semantic search would do.

### What's New

- **Claude Code Hooks** — Optional hooks that automatically redirect local file searches to ContextStream's semantic search. Your AI gets better results faster, and you save tokens. Install with `npx -y @contextstream/mcp-server setup` or `generate_rules(editors=["claude"])`.

- **Smarter Reminders** — The API now reminds your AI to search ContextStream first, every time. Even if instructions drift during long conversations, the reminders keep it on track.

- **Lessons That Stick** — Made a mistake once? ContextStream surfaces relevant lessons before your AI repeats it. Past corrections now actively prevent future errors.

- **Automatic Update Prompts** — When your rules or MCP server version falls behind, you'll get a clear nudge to update. Updates are safe—your custom rules are preserved.

- **Notion Project Support** — Pages created via the Notion integration now link to your current project for better organization.

### Upgrading

```bash
npm update @contextstream/mcp-server
```

Or re-run setup to get the latest hooks:

```bash
npx -y @contextstream/mcp-server setup
```
