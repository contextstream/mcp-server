# Changelog

## 1.0.14

- macOS: the launchd-managed sync bridge no longer stays down or respawns
  thousands of times after `contextstream-mcp update`. Updates re-sign the staged
  binary with a stable identifier (`io.contextstream.mcp`), so launchd's cached
  launch constraints still match. A version change reloads the job (bootout, then
  bootstrap, retrying bootstrap briefly) instead of `launchctl kickstart -k`. A
  watcher running under the managed launchd label now waits for the singleton
  lock and takes over when it is free, instead of exiting at once and being
  respawned every ten seconds.
- Session: `resume` and `resume_list` leave out the caller's own session by
  default, so a fresh session's `resume` returns the previous session and not
  itself. The excluded id is the explicit `session_id`, then the id the server
  was initialized with, then the transport's MCP session id.
- `vcs`: `create_link` works. It sent `source_*` and `target_*` as the request
  field names, where the API requires `vcs_object_type`, `vcs_object_id`,
  `cs_object_type`, and `cs_object_id`, so the API refused every call with
  "missing field `vcs_object_type`". The parameters are unchanged (`source_*` is
  the VCS object, `target_*` the ContextStream object). Both ids must be UUIDs and
  are checked before the request is sent.

## 1.0.13

- Session: `session(action="resume_list")` lists the caller's recent sessions,
  newest first, and `session(action="resume")` loads one session's card (the
  newest session that did real work when no `resume_id` is given), so a fresh
  session can pick up recent work without a handoff. Both call the ContextStream
  API and show its rendered text. The `session` tool's input schema gains these
  two actions and a `resume_id` parameter; tool names and descriptions are
  unchanged.
- Hooks: Codex and Claude prompt hooks receive the canonical context output
  schema. Cursor is now recognized by its own event names, instead of by the
  absence of `tool_name`, which also matched Codex and Claude prompt events.
- Hooks: global ignore rules are matched against the checkout being indexed, so a
  prompt hook draining another checkout no longer panics.
- `context()` shrinks `grounding_hits`, `instructions`, `coordination_inbox`,
  `matched_skills`, and `lessons` to fit the wire budget before dropping them.
  Ids, handles, paths, urls, and kind, type, and status strings are never
  truncated. The budget report gains `shrunk_structured_field_count`.

## 1.0.12

- Security: rustls 0.23.45 and rustls-webpki 0.103.15 replace 0.23.40 and
  0.103.13, which RUSTSEC-2026-0285 flags. rustls accepted TLS 1.3 handshake
  messages sent at the wrong encryption level. The advisory is medium severity
  (5.3) and does not let a network attacker alter or complete a handshake.
- Hooks: initialization is tracked per host session, so starting or resuming a
  second session in the same checkout no longer blocks an initialized session
  with "First call required". A successful `init` or `context` result completes
  initialization, and the managed post-tool matcher now includes `context` and
  `session` (#144).
- An empty project no longer reports `canonical_index_ready`: a reported file
  count of 0 now outranks a "ready" label, so agents are not sent to an empty
  search (#145).
- Shell code search (`rg`, `grep -r`, `find -name`) in a checkout with no
  recorded index gets a non-blocking nudge to run `project(action="index")`, at
  most once every 10 minutes per checkout (#145).
- `context()` keeps `instructions`, `matched_skills`, `coordination_inbox`, and
  `grounding_hits` under the wire budget, trimming the large duplicate fields
  first (#145).
- Global rules are written without a workspace identity, so setup runs in
  different directories no longer leave different workspaces in the one global
  file (#145).
- `testing/adoption` measures ContextStream search against shell search from
  local agent transcripts (#145).
- Dependencies: hyper-util 0.1.21, tiktoken-rs 0.12.1, ignore 0.4.33,
  tokio-test 0.4.6, and console 0.16.6 (#139). CI action pins updated (#140).

## 1.0.11

- Optional deep project learning starts off. Interactive setup offers a separate review,
  while plain `setup --yes` preserves the account’s choice. `setup` and `configure`
  accept `--account-learning on|off`: on opens the dashboard for human consent; off
  withdraws directly. Doctor and init report status without opting anyone in
  (#141, #142).
- The README quick start uses the plain `curl` install command (#138).

## 1.0.10

- Dependencies: Rust dependencies updated, including axum 0.8, schemars 1.2,
  notify 8, jsonwebtoken 11, tower-http 0.7, dirs 7, dialoguer 0.12, and
  toml_edit 0.25 (#90). The MCP wire contract is unchanged from 1.0.9: the
  same 35 tools with identical names, descriptions, and input schemas, and the
  same HTTP routes.
- The npm "Homepage" link and the README web link now open the MCP landing
  page, and the README shows the hosted endpoint as
  `https://mcp.contextstream.io/mcp?default_context_mode=fast` (#136).

## 1.0.9

- Setup: configures Muse Code, Kimi Code CLI, ZCode, Qwen Code, Gemini CLI,
  Zed, Claude Desktop, GitHub Copilot CLI, Factory Droid, Amp, and Crush, 22
  clients in all. Each gets its MCP config, instruction file, doctor checks,
  and detection; Claude Desktop gets the local binary because its config file
  only launches local servers (#129).
- Setup: global configs land where each client reads them. Claude Code's user
  scope moves to `~/.claude.json` (it never read `~/.claude/mcp.json`),
  OpenCode to `~/.config/opencode/opencode.json`, and Antigravity to
  `~/.gemini/config/mcp_config.json`. GitHub Copilot CLI is its own client.
  Roo Code, which has shut down, is repaired and removed but no longer offered
  (#127).
- `contextstream-mcp clients --format json|markdown` prints every supported
  client with its config paths, key path, and entry shape (#128, #133).
  `docs/clients.md` lists them with one-line add commands and a VS Code
  one-click install (#131).
- Tools: every advertised tool schema uses only keywords all major model
  providers accept, and long tool descriptions are summarized to at most 1024
  characters with the full text kept on the main parameter. Kimi, GLM, Qwen,
  Gemini, and OpenAI-compatible endpoints no longer reject ContextStream's tools
  (#126).
- Models: session analytics recognize GLM, Qwen, DeepSeek, MiniMax, newer Kimi,
  Gemini 3, and Muse Spark, including vendor-prefixed ids such as
  `openrouter/z-ai/glm-4.6`, and size context-pressure warnings to their
  windows. Cursor, Codex, and VS Code are recognized by the names they report
  at initialize (#130).
- Hosted: the connection's client is forwarded to the API, so guidance names
  tools the way that client does, and unrecognized client names are logged for
  future support (#132, #134).
- Hosted: the tenant-home scope header is relayed beside the home region it
  describes (#125).
- Search: the retired search-learning consent is no longer offered or
  forwarded (#124).
- Search reports an index's age from the committed-index time when result
  rows carry no ingest time, so an index built on another machine is no longer
  labelled "recent" while days old (#123).

## 1.0.8

- Setup: onboarding takes one review screen. Setup reuses a working saved
  sign-in, accepts detected editors, picks or creates the workspace, links the
  folder's project or Git repository (or plans a new project created only on
  Save), and indexes in the background. Setup, doctor, update, and index screens
  use the ContextCode terminal design, with fallbacks for 256/16-color
  terminals, `NO_COLOR`, and legacy Windows consoles (#115).
- Indexing: background indexing started by `setup` or `index --background` now
  runs in a detached worker and finishes after the command exits (#115).
- Project Brief: `init` shows the project's evidence-cited brief from the
  session init response, framed as reference data. `project` gains `brief`,
  `brief_update`, and `brief_refresh` (#116).
- Sync bridge: a checkout whose account has no write access (403) or whose
  credentials are rejected (401) is no longer re-submitted every 1.5 s. It
  warns once, keeps local changes queued, and rechecks after 10 then 30 minutes
  or on a bridge reload. A 429 waits for its `Retry-After`, and timeouts and
  5xx back off from 2 s to 5 minutes. The client now honours `Retry-After`
  without `X-RateLimit-*` headers, hands long waits back to the caller instead
  of sleeping inside a request, and a denied full scan stops after the first
  refused batch. Search-triggered repairs hold the same way (#120).
- Performance: stdio `initialize` answers in ~4 ms instead of ~85 ms, editor
  hooks run in ~3 ms instead of ~12 ms, no-op hooks are no longer installed, and
  `init` overlaps independent lookups (#114).
- `generate-configs --help` no longer prints the value of
  `CONTEXTSTREAM_API_KEY` (#105).
- `setup --yes` accepts `--workspace-id` and `CONTEXTSTREAM_WORKSPACE_ID` for
  accounts with several workspaces (#106).
- `qa` reads the API's renamed `upstream_*` latency fields (#107).
- `configure --api-key-stdin` saves an existing key without the browser flow
  (#109).
- `answer` explains Answer API scope and lane refusals instead of returning a
  bare 403/409/422 (#110).
- Install URLs point at `https://contextstream.io/scripts/mcp.sh`; the README
  one-liner and `update` fail when the download fails (#104, #108, #112).

## 1.0.3

- Grounding: preserve evidence provenance, distinguish ranking signals from
  relevance, and admit relevant evidence before canonical deduplication.
- Context caching: bypass composite cache reuse when checkout or session state
  requires fresh grounding.
- Add offline grounding replay and scoped qualification controls. Candidate
  ranking remains shadow-only by default; serving changes require explicit
  operator configuration and independently qualified evidence.

## 1.0.2

- fix(coordination): `coordination(action=share)` validates `kind` against the API enum (decision, constraint, api_contract, risk, status, knowledge) and defaults an omitted kind to `knowledge` like the API; 1.0.1 rejected `status`/`risk` client-side and sent an API-refused `note` default (#88).

## 1.0.1

- Answer: `answer` actions for query, recent changes, receipt recovery, and
  feedback against the ContextStream Answer API. Mutations are one-shot with
  no retry or session-refresh replay; request and receipt identity are
  validated with strict closed schemas, and recorded-only feedback is
  reported as such (#83).

- Decision conflicts: the API flags a decision capture that overlaps another
  session's recent decision on the same subject (`possible_conflicts`, either
  direction). `[DECISIONS]` / `[RECENT_DECISIONS]` lines carry a
  `⚠️ possible conflict with "…"` note plus a `[DECISION_CONFLICT]` rule,
  `[GROUNDING]` hits get a `conflict-check` label and a `DECISION_CONFLICT`
  line naming the other decision, the session-start and subagent-start
  briefings show the note, and `session(action="capture")` announces it so
  the agent confirms with the user which decision stands and supersedes the
  other. Older API builds render nothing extra.
- Coordination v2: `coordination(action="reply", notice_id, message)` answers
  a notice back to the session that raised it (identical replies dedup;
  fan-in and ended-session notices have no origin and are reported as "ack or
  dismiss instead"); `check_in` forwards an optional `metadata` object (git
  branch/commit) as presence metadata the coordination judge sees as
  evidence; a `(blocking)` notice is a direct conflict with another session,
  to be read before continuing and then acked or replied to.

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
