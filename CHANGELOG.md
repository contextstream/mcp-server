# Changelog

## Unreleased

### Support and discovery

- Documented the hosted Rust, installed Rust, legacy npm TypeScript, and Desktop release lines, including how `help(action="version")` maps each MCP runtime to its release metadata.
- Added legacy parity for `session(action="list_recaps")` and `session(action="trigger_recap")`; recap history now exposes `recap_date` and `generated_at`, and the tool guidance correctly describes the nightly local-time schedule rather than a session-boundary trigger.
- Added an explicit `runtime_type: legacy-typescript-mcp` field to legacy version output so agents can distinguish the npm runtime from the hosted Rust MCP.

## 0.4.81

**Setup wizard: zero-prompt `--yes`, background project setup, doctor verification — plus cross-component interop fixes.**

### Setup Wizard

- **`setup --yes` (or `-y`)** runs the entire wizard with zero prompts: every prompt's documented default applies. The outdated-version gate proceeds with a warning instead of exiting; missing credentials fail fast with an actionable message (set `CONTEXTSTREAM_API_KEY` or run setup interactively once); a single listed workspace is auto-selected (multiple → skip); undetected editors are skipped. `--editors=<csv>` pre-answers the editor selection (and force-configures the named editors even when undetected); `--no-doctor` skips final verification.
- **Project setup never blocks onboarding:** indexing now runs in a detached child process and the wizard returns immediately — search works right away (keyword first, semantic as the index builds), with progress available via `project(action="index_status")` or `contextstream-mcp doctor`. The indexing section is retitled PROJECT SETUP.
- **The wizard ends with VERIFYING YOUR SETUP:** the `doctor` diagnostics run inline against the just-configured auth, so any misconfiguration surfaces with its next-step hint while you're still in the terminal. Findings are advisory — setup still exits 0.
- **New `contextstream-mcp index [path]` command** — previously the skip-hint referenced a command that did not exist; it now indexes a folder on demand and is the target of the wizard's background child.

### Cross-Component Interop

- `readSavedCredentials` accepts the legacy `{api_key, saved_at}` credentials shape (no `version`/`api_url`) — a key saved by one ContextStream component now works in all of them.
- Folder-mapping resolution understands exact-folder entries (`{path, workspace_id, project_id, …}`) written by other components alongside glob patterns, carries their project scope through `resolveWorkspace`, and skips malformed entries instead of crashing.
- `doctor` recognizes the setup wizard's managed-rules markers.

## 0.4.80 (continued) — parity passes 6–8

**Rust MCP parity passes 6–8 (final): deep search, editor surfaces, polish audit.** Folded into the unpublished 0.4.80 release.

### Deep Search (v0.3.34 + v0.5.10 portable subsets)

- **Post-rank token fusion** (semantic/hybrid): results are re-scored by query-token hits in content/path/location plus exact-query and path boosts, so exact matches surface above merely-similar hits; reorders are noted and recorded.
- **Adaptive retry thresholds:** identifier/symbol queries accept lower-confidence hybrid results before a semantic retry (0.4/0.48), natural-language questions demand more (0.6); semantic displaces hybrid for code-shaped queries only when it wins by a wider margin (0.04).
- Result lines carry a **confidence band** (high/medium/low) next to the score, and structured output records `fallback_stages` (requested/executed mode, token fusion, server rewrite recovery).
- **Auto-heal stale index roots:** when a folder has no local index binding but a recorded binding elsewhere shares its exact git remote identity (repo moved/renamed/recloned), both folder-to-project binders adopt that project here and refresh the binding. Name/path-leaf similarity never binds.

### Editor Surfaces (v0.5.25 + v0.5.27 subsets)

- **Cursor rules** now write to `.cursor/rules/contextstream.mdc` with `alwaysApply` frontmatter — Cursor Agent mode does not read `.cursorrules` at all, so managed rules were invisible exactly where they mattered most. Existing `.cursorrules` blocks are left in place for older Cursor versions but are no longer created.
- **Cursor hook responses** use the current permission schema (`{permission, agent_message, user_message}`) instead of the ignored legacy `{decision, reason}` shape; the Cursor hook installer appends `--editor=cursor` so hook runtimes can tell Cursor apart from Claude Code.
- **New `contextstream-mcp doctor` command:** read-only diagnostics across auth, API reachability, folder scope binding, local index health (git identity/HEAD drift included), editor rule files, and installed hooks — one ✓/✗ line per check with a next-step hint, and it runs without credentials (diagnosing missing auth is part of the job).

### Verified Without Change / Deferred

- Media list/search summaries (v0.3.69/70): the media tool already renders actionable formatted output for both; only the download-URL enrichment for top items is unported.
- v0.3.27 per-call display titles and icon metadata: the substantive parts (per-action tools, branded titles) shipped in 0.4.76; transport-level call-title/icon cosmetics depend on MCP client/SDK support and stay deferred.
- Setup `--yes` zero-prompt path (v0.5.27): the interactive wizard's prompts are call-site-specific; a faithful non-interactive path needs its own pass.
- Cursor schemas for non-permission hooks (sessionStart `additional_context` casing, `postToolUse` spec): the deny/allow contract — the inert steering channel — is fixed; the remainder is tracked.

## 0.4.80

**Rust MCP parity pass 5: session-state cluster.**

### session `retro_capture`

- After-the-fact decision/note/snapshot capture from prior work with source provenance. Takes a title plus at least one of `content`, `query`, or `transcript_id(s)`: sources are collected from recall and transcript reads, deduplicated, and assembled into the event content as numbered `[kind] title (id) — date` evidence with a `Source query:` line. Provenance records `source=mcp_retro_capture`, the source query, transcript ids, and source snippets; source-lookup failures are tolerated only when manual content was provided (recorded as `provenance.source_lookup_error`). The structured result echoes a `retro_capture` block (source query, transcript ids, count, results). Plan event types are rejected — `capture_plan` owns plans. Participates in write-scope resolution and one-shot scope recovery.

### session `set_account_mode`

- Validates `team | personal | auto`; team/personal select the account context via the public account-context endpoint (auto only reads the current snapshot), and the result renders an `[ACCOUNT_CONTEXT]` block. Note: per-call `account_mode` overrides on other tools remain unported (they depend on execution-state plumbing this package does not carry).

## 0.4.79

**Rust MCP parity pass 4: metadata/UX polish cluster (v0.3.11–v0.5.27).**

### Grounding Quality (v0.3.29 + v0.5.27)

- Ground-bundle items never render as bare "Untitled": display titles fall back from title → summary → first meaningful content line → a typed id.
- Operational hook telemetry (`operation`/`command_execution`/`file_operation`) older than 7 days — unknown age counts as stale — is dropped from the ground bundle so months-old telemetry cannot bury real prior work. Hook captures already posted `event_type=operation` (verified, no change needed).

### Model Awareness (v0.3.44 + v0.3.49 subsets)

- When the MCP client surfaces its model at initialize, API requests carry `X-ContextStream-Model` so server-side events attribute to the real model; the header is omitted when the model is unknown (existing behavior).
- Known 1M-window models raise the context-pressure threshold to ~650k instead of the conservative 70k default; unknown/older models keep the default exactly.

### Tier Gating (v0.3.38)

- Tier-gated tools return a structured `plan_restricted` block (current plan, required tier, upgrade URL) alongside the text nudge, so clients can render an upgrade prompt without parsing error text.

### Tool-Description Teaching (v0.3.11 + v0.3.13)

- The memory tool description now teaches that docs/runbooks/specs/decisions live in memory (never on disk) and routes tickets/bugs/incidents/releases to the `entity` tool.

### Verified Without Change

- Full-body rendering in `get_task`/`get_event`/`get_todo`/`get_transcript` (v0.3.28): the TS renderer never truncated.
- Hook telemetry classification as `operation` (v0.5.27 hook half): already the default in the shared hook capture helper.

### Deferred

- v0.3.27 icon metadata (niche MCP client support), v0.3.40 freshness-aware grounding (server-rendered), v0.3.69/70 media summary polish.

## 0.4.78

**Rust MCP parity pass 3: search-quality cluster (v0.3.45–v0.5.26).**

### Search-First Contract (v0.5.23)

- Every agent-facing surface that permitted a preemptive "index not ready → use local tools" fallback (generated rules blocks, hook reminder text) now states the reactive-only contract: run `search(...)` first even while the index is still building (keyword hits return immediately); fall back to local tools ONLY after search itself returns 0 results/errors on a retry, for known-new edits, or on explicit user request. The rules version tracks the package version, so managed rule blocks regenerate on next run.

### Code-Intent Gating (v0.3.52)

- Identifier-shaped queries (snake_case, camelCase, PascalCase, `::`/dotted member access — even as a lone token) suppress memory/doc blending on workspace-scoped searches, so a symbol lookup no longer ranks docs or branding assets alongside code. Conservative: prose and plain memory words are unaffected. The consolidated `search` tool now exposes `include_memory` for explicit opt-in/out.

### Zero-Result Rewrite Recovery (v0.5.26)

- When the backend recovers an empty natural-language search by retrying query rewrites, the output carries an honest provenance line ("0 direct hits — recovered via server-side rewrites: …") and a `rewrite_recovery` structured block. Older backends omit the fields and behavior is unchanged.

### Freshness Guard (v0.3.45 + v0.3.63)

- Search results whose local file no longer exists under the active folder are pruned, with a note.
- A non-silent `[INDEX_HEALTH]` advisory reports how many tracked files changed since the last index (git-status based, mtime-gated against the recorded `indexed_at`, bounded and best-effort). IndexKeeper's incremental pass continues to re-ingest changed files in the background; this pass adds the visibility.

### Deferred (need their own pass)

- v0.3.34 "untouchable search quality v2" ranking/cache-freshness overhaul and v0.5.10 stale-index-root auto-heal: each is a ~400-line search-engine-side rework in the Rust tree; the TS server already carries partial equivalents from 0.4.72 (mode escalation, artifact filtering, ripgrep pre-fetch, IndexKeeper). Porting the remainder faithfully needs a dedicated pass.

## 0.4.77

**Rust MCP parity pass 2: scope & routing correctness (v0.3.6–v0.5.18 cluster).**

### Central Scope Resolution (new `src/scope.ts`)

- Write paths — memory `create_event`/`create_node`/`create_task`/`create_todo`/`create_diagram`/`create_doc`/`create_roadmap`/`import_batch`, session `capture`/`capture_lesson`/`capture_plan`/`remember`, and all flat write tools — now resolve a consistent `{workspace, project}` pair before the first attempt. A "soft" workspace (neither caller-provided, nor request-header-provided, nor backed by the folder's saved config) that disagrees with the candidate project's real workspace is self-healed by adopting the project's workspace and persisting the correction; explicit workspaces stay authoritative and a mismatched project is dropped with a `[SCOPE]` note instead (Rust v0.5.18 semantics).
- Project **names** are accepted wherever a `project_id` is taken on these paths (and on `init`): a non-UUID value is matched case-insensitively (ignoring spaces/underscores/hyphens) against the workspace's projects, with a helpful error naming known projects on a miss (v0.3.7).
- One-shot recovery after stale-scope errors (v0.3.53–v0.3.58): a rejected project retries workspace-only; a stale workspace is cleared (new `SessionManager.replaceScope` + `ContextStreamClient.clearDefaults`) and re-resolved from the folder's saved config, with the retried call re-running against the corrected session context and a `[SCOPE]` note explaining what changed. Applies to memory/session/entity actions and the flat write tools, including the `memory decisions` read path.
- An explicitly requested but inaccessible project is a hard error instead of a silent mis-scope.

### init Precedence (v0.3.9 + v0.5.7)

- `init(folder_path=…)` suppresses header-injected workspace/project scope so folder binding actually runs; when folder resolution yields no workspace (typical through the HTTP gateway, where the server cannot read the caller's local config), the inherited header scope is restored as the fallback and noted in the result.
- `init` `project_id` accepts a project name.

### Plan Lookup Hardening (v0.5.14)

- `get_plan`/`update_plan` never dead-end: `plan_id` accepts a UUID or title text, or can be omitted to auto-resolve the latest actionable plan — preferring substantive plans (progress, active status, linked tasks) over stray 0% drafts, and disclosing the auto-pick plus other actionable plans. A miss returns the in-scope plan listing; an empty scope points at `capture_plan`.
- `capture_plan` (session action and flat tool) rejects degenerate step titles (empty, placeholder dashes, multi-line, >200 chars, bare file paths) with per-title reasons.

### Search Scope Loudness + Local Index Identity (v0.5.7 + v0.5.17)

- `[SCOPE_UNRELIABLE]` banner when the backend flags a search's scope as invalid (previously skipped silently).
- The local index registry records the git HEAD and normalized git remote identity at local-ingest time. Folder-to-project binding requires the folder's current remote identity to match the recorded one — a repo that merely shares a name or path leaf can never bind to another repo's project. IndexKeeper detects out-of-session commits (HEAD moved past the recorded ingest HEAD) and starts a background re-ingest. Non-git folders and entries without recorded identity are unaffected.

### Already Equivalent (verified, no change)

- Entity list/create inherit the session's active scope (v0.3.10); repeated `init(folder_path=…)` re-binds; per-initialize tool-surface profile reset (v0.5.16).

### Still Tracked for Later Passes

- Search-quality behaviors (v0.3.34, v0.3.45, v0.3.52, v0.3.63, v0.5.10, v0.5.23, v0.5.26); grounding freshness + metadata polish (v0.3.27–29, v0.3.40, v0.3.44, v0.3.49, v0.5.27); session `retro_capture`/`set_account_mode`.

## 0.4.76

**Rust MCP parity (v0.2.92 → v0.5.27), pass 1: tool-surface parity + confidentiality scrub.**

### Confidentiality Scrub (parity with Rust v0.3.2–v0.3.4, v0.3.12, v0.5.15)

- Removed internal implementation naming from user-facing strings and comments: archive-tier wording in the memory tool description and the `search_archive` local-unavailable response, cache naming in hook comments. The fabricated `stages_used` marker is now `hosted_archive`.
- Dropped the `ram`/`mem` compatibility aliases for `instruct` (the Rust MCP dropped them in v0.3.2). `flash` remains.
- Tool error display no longer echoes raw API error bodies: an allowlist keeps known-actionable fields (code, message, validation issues, rate-limit info, request id), messages are length-capped, and stack traces are stripped.

### New Tool Surfaces

- **`qa` tool** (Rust v0.3.1/v0.3.5): agent Q&A over the workspace/project knowledge base via the public `/qa_agent/*` endpoints — `ask` (grounded answer with citations + confidence), `search` (prior Q&A listing), `save_kb`/`list_kb`/`get_kb`/`update_kb`/`delete_kb` (guidance/guardrail/faq/runbook/caveat items), `feedback` (-1/0/+1). Structured answers surface only public answer-contract fields and always attribute answers to ContextCode.
- **Per-action write surfaces**: `capture_plan`, `session_capture`, `session_capture_lesson`, `session_remember`, and `memory_create_event` are now part of the default (consolidated) and standard toolsets, joined by new `memory_create_doc`, `memory_update_doc`, `memory_delete_doc`, `memory_create_task`, `memory_update_task`, `memory_create_todo`, and `memory_complete_todo` tools that dispatch to the consolidated memory handler, so clients that render only tool names get descriptive write tools.
- **`skill`, `vcs`, and `integration` join the standard toolset** — previously registered but invisible in the default surface (same fix class as Rust v0.3.5, which surfaced qa/entity/vcs through the standard/consolidated toolsets).

### New Actions on Existing Tools

- **session:** `update_lesson` / `delete_lesson` (v0.3.30) with UUID-or-title lesson resolution — exact title matches win, ambiguous lookups are refused with candidates listed.
- **memory:** `delete_all` (v0.5.24) — `delete_event`/`delete_node` accept an exact title in place of a UUID; multiple matches error unless `delete_all=true` removes all exact-title matches in one call.
- **capsule:** `delete` (v0.5.21); `share` accepts `require_unlock_key` + `unlock_destinations` (v0.3.48).
- **project:** `purge` (de-index, keep project record), `remove_paths` (de-index exact paths), `forget_local` (remove the folder's local scope binding only; server data untouched), `merge` (merge source project into target).
- **skill:** `supersede` (v0.3.51) — archives the skill with a change summary so it stops surfacing in matches.

### Verified Without Change

- Per-initialize tool-surface profile reset (Rust v0.5.16): the initialize interceptor already resets the active profile to the construction-time default on every initialize, so a prior client's auto-detected narrowing cannot bleed into the next client.

### Not Ported Yet (tracked follow-ups)

- Scope & routing recovery cluster (v0.3.6–v0.3.10, v0.3.53–v0.3.58, v0.5.7, v0.5.14, v0.5.17–v0.5.18), search-behavior parity (v0.3.34, v0.3.45, v0.3.52, v0.3.63, v0.5.10, v0.5.23, v0.5.26), grounding freshness + metadata polish (v0.3.27–v0.3.29, v0.3.40, v0.3.44, v0.3.49, v0.5.27), and session `retro_capture` / `set_account_mode` (require session-state machinery this package does not have yet).
- Hosted-only premium surfaces (`chart`, `async_job`) are intentionally not exposed: they are not part of the local Rust stdio build surface either.

### Tooling

- `scripts/registration-smoke.mts`: registration smoke test asserting the default consolidated surface (required tool names present; `ram`/`mem`/`chart`/`async_job` absent).

## 0.4.75

### Security

- Bumped transitive `hono` dependency to 4.12.14 to resolve GHSA-458j-xx4x-4375 (moderate — improper JSX attribute-name handling in `hono/jsx` SSR).

### Rust MCP Parity (v0.2.46 → v0.2.57 + 6a43ded)

- **Search scoring thresholds (v0.2.46):** Raised `HYBRID_LOW_CONFIDENCE_SCORE` from 0.35 → 0.55 and lowered `SEMANTIC_SWITCH_MIN_IMPROVEMENT` from 0.08 → 0.02 so mediocre hybrid results trigger semantic fallback and semantic wins don't need to massively outperform hybrid.
- **Semantic retry for NL queries (v0.2.46):** `shouldRetrySemanticFallback` now allows NL queries that route to `hybrid` (e.g. containing UI component terms like "page" or "layout") to retry with semantic. Skips only clearly structural modes (`pattern`/`exhaustive`/`refactor`/`team`) and identifier queries. (Server-side keyword sub-token demotion lives in the backend search engine and is not mirrored client-side.)
- **Null workspace_id guard (v0.2.47):** `createMemoryEvent` now returns the clearer "workspace_id is required for session capture but was not set. Run init first." error before issuing the API call. `session_id` is forwarded to the event body root in addition to metadata.
- **Rules block refresh (v0.2.51 / v0.2.52 / v0.2.53 / v0.2.57 / 6a43ded):** Regenerated rules now include a "Common queries" quick-reference, a "Skills, Docs & Lessons First" block, a "Project Scope Discipline" block, a "Past Sessions Are Queryable" block with exact tool calls, an expanded "Memory, Docs, Lessons & Decisions" guidance block with explicit local-file warnings, an updated `[MATCHED_SKILLS]`/`[LESSONS_WARNING]` notices row, and `save lesson` / `save decision` rows to prevent agents from writing lesson/decision markdown to local files invisible to the surfacing pipeline.
- **Session tool description (6a43ded):** Promoted `capture_lesson` and `recall` to the first sentences of the `session` tool description so LLMs scanning tool descriptions immediately see "LESSONS LIVE HERE" and "PAST SESSIONS LIVE HERE" before the comma-separated action list.
- **Past Sessions banner (v0.2.57):** SessionStart hook additional context now includes a "📜 Past Sessions Are Queryable" banner with exact tool calls (`session(action="recall")`, `memory(action="list_transcripts")`, `memory(action="search_transcripts")`) so agents check transcripts before asking the user what happened previously.

### Issue Remediation

- **#53 Truncated UUID prefixes:** Added `validateIdOrPrefixHint` helper and wired it into the `session` and `memory` tool handlers. Truncated UUID-shaped inputs (8–35 hex-ish chars) now return a targeted error identifying the offending field and explaining that prefix resolution isn't supported. `event_id`, `plan_id`, `task_id`, `node_id`, `todo_id`, `diagram_id`, `transcript_id`, `lesson_id`, and `suggestion_id` schema validators were relaxed from `.uuid()` to `string()` so the friendly handler-level error fires instead of a generic Zod "Invalid uuid" message. Full prefix resolution remains a backend concern.
- **#54 Top-level agent/mode metadata:** `session(action="capture" | "capture_lesson")` no longer pollutes event content with `[Agent: X | Mode: Y]` headers. `agent` and `mode` are now forwarded as structured top-level fields on the `/memory/events` request, stored in event `metadata`, and preserved as the `agent:<name>` / `mode:<value>` tag convention for backward-compatible filtering. `memory(action="list_events", agent, mode)` accepts the same structured filters and translates them into tag queries plus a client-side post-filter that matches either the tag or the structured field.

### Cross-Repo Ownership

- Issues tied to downstream Desktop/Web/backend products (Windows updater binary, dashboard re-index button, dashboard version display, Atlas knowledge graph visualization, `graph(dependencies)` engine timeout) were filed in their owning repositories. No code changes for those are landed here.

## 0.4.76 (continued) — previously unreleased fixes

### Parity + Issue Remediation

- **Skills default to active on save (Slack report):** `skill(action="create")` now sends `status: "active"` by default so skills saved through the MCP are usable immediately instead of landing as "Draft" in the dashboard. An explicit `status: "draft"` (or `"archived"`) at save time is still honored. `createSkill` now accepts an optional `status` parameter and the tool layer forwards `input.status`.
- Added adaptive ingest behavior for 413 payload errors with recursive batch splitting, conservative payload limits, and oversized serialized-file skipping.
- Wired `project(action="files")` pagination/filter arguments through to the API client and improved `project(action="index_status")` diagnostics when pending file paths are not returned by the backend.
- Improved session resiliency: `getHighPriorityLessons`, `getContextSummary`, `getContextDelta`, and `decision_trace` now log actionable fallback diagnostics and continue with fallback flows where possible.
- Added compatibility metadata support for `session(action="capture", agent, mode)` by encoding these values into tags/content so current APIs remain queryable.
- Clarified graph timeout failures with targeted remediation hints for `graph(action="dependencies")`.

### Ownership Notes

- Issues tied to Desktop/Web products (e.g. Windows updater binary packaging, dashboard buttons/version display, Atlas visualization UI) require changes outside this repository.
- This repository now surfaces clearer diagnostics for those backend/UI-coupled cases, but full resolution for those issues remains in the owning product repositories/services.

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
