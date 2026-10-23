# Changelog

## 1.0.29

- Hosted gateway: the tenant home the API names on a response
  (`x-contextstream-tenant-home-region`) is relayed for every region the API
  can name, `us-virginia` and `us-oregon` included. It was parsed with the
  three-name vocabulary the edge uses to suggest a home for a new tenant, so
  the home of every tenant on the AWS regions was dropped and the edge never
  learned it: an Oregon-homed tenant's calls kept going to the default
  (Virginia) origin. The suggestion for new tenants is unchanged.
- `contextstream-mcp update` installs where the running binary lives when that
  directory is writable, verifies the binary that reports the new version
  wherever the installer put it, and never deletes a copy that already reports
  the new version. It used to install to `/usr/local/bin` only, fail without
  sudo, and then remove a fresh `~/.local/bin` install as stale.
- Setup keeps a project `.mcp.json` that carries an API key out of git: when
  the written entry has a key it adds the file to `.git/info/exclude` (local
  only, never `.gitignore`) unless git already tracks or ignores it.
- The Claude Code prompt hook sends the prompt (collapsed, at most 500
  characters) as `task_summary` and, once init has named one, the agent's
  ContextStream session as `coordination_session_id`, so the session checks in
  for live coordination on every turn and pending notices show on the next
  one. `CONTEXTSTREAM_COORDINATION_CHECKIN=false` keeps prompts out of the
  hook request.
- No tool is added or removed.

## 1.0.28

- A gateway that calls the ContextStream API directly can now present the API's
  forwarder secret. Set `CONTEXTSTREAM_API_FORWARDER_SECRET` and every request
  the client sends to the configured API URL (reads and writes, through any
  of the client's three request paths) carries it in the `x-contextmail-forwarder`
  header. Until now such a gateway, for example the one that runs beside an API
  color and calls the app on loopback, got `403 {"error":"forbidden"}` on every
  call, because the API's origin gate wants that header and only the edge
  adds it. The variable is opt-in: unset, empty or blank means no header and no
  change. The value is trimmed, read once when the first request is built, sent
  as a sensitive header (it is redacted in debug output) and never logged. A
  value that is not a legal header value is not sent and one warning names the
  variable, not the value. Nothing else reads it, so a gateway behind the edge,
  and every stdio install, needs no setting. Device login and sign-up requests
  do not carry the header (#192).
- Nothing else changed: no tool, schema or description differs from 1.0.27, and
  there are no dependency changes.

## 1.0.27

- Security: TLS trust. `rustls-webpki` 0.103.15 to 0.103.16
  (RUSTSEC-2026-0382) and `webpki-roots` 1.0.7 to 1.0.10 (RUSTSEC-2026-0381).
  The earlier versions can accept certificate paths through distrusted emSign
  intermediates whose private keys are now known, and the older root list
  includes `emSign ECC Root CA - G3`. Both crates are compiled into the binary
  through `reqwest`, so 1.0.26 carries the affected versions. Update with
  `contextstream-mcp update` (fe2ed5f).

- The hosted HTTP gateway now serves `answer`, `feed`, `vcs`, `capsule`,
  `chart` and `async_job` to every client. The gateway built its startup tool
  registry from a list of its own that never gained these six, while the
  per-request registries (a config override, the stateless transport) used
  the shared `build_registry`. `tools/list` was cached once per process under a
  key that did not say which registry built it, so whichever request came first
  decided the list until the next restart: a gateway could list the six and
  then answer `Unknown tool` when a session client called them, or not list
  them at all after a restart. The gateway now uses `build_registry`, and the
  cache key includes the registry's tool names.
- Local Git capture hooks run again in repositories that had no hook before
  setup. Up to 1.0.26, setup created a new `post-commit`, `pre-push`,
  `post-checkout` or `post-merge` hook with mode 0600, so Git skipped it and
  no commit, push, checkout or merge reached ContextStream. New managed hooks
  are now created 0755, and `setup`, `init`, `update`,
  `contextstream-mcp git-hooks` and `doctor --repair --scope project` restore
  the execute bit on a managed hook that lacks it (`doctor --repair` still
  needs a configured editor or `--editors`, and leaves a `.contextstream.bak`
  copy of each hook it repairs). `doctor` has a new `git_hooks` check that
  fails when a managed hook is not executable (#179).
- Search no longer returns every file twice after `project(action="ingest_local")`
  or the background refresh that `init` starts. The index also holds a
  file-level summary row for each file at line 0. When the same file (in the
  same project) also has a chunk row, the summary row is folded into it: the
  chunk row keeps the better rank and score, and the header count drops by
  one for each row folded. A file-level row with no chunk row is shown as
  `path (file-level match)` instead of `path:0`. When the API's `total`
  counts rows it did not return on a short last page (semantic search said
  "4 results" and showed 2), the header shows the rows actually returned
  (#183).
- `.contextstream/ignore` now excludes files from indexing, as the setup
  privacy screen and `docs/data-handling.md` have said. Until now ingest
  read only `.contextignore` and `.p4ignore`, so files excluded the documented
  way were uploaded and searchable. The file holds gitignore-style rules
  relative to the project root and applies as its own filter, like Plastic's
  `ignore.conf`, so no `!` rule in another ignore file can re-include what it
  excludes. Every ingest path honours it: setup's background index, `init`'s
  auto-index, `project(action="index"|"ingest_local")`, the hook drain and
  the file watcher. A file that an ignore rule now excludes but that an
  earlier index uploaded is dropped by the next complete index, as the
  Unity `Library/` files were in 1.0.26. Keyword search's local disk
  fallback (ripgrep or the built-in walker) applies the same ignore rules as
  ingest, so content excluded by `.contextignore`, `.contextstream/ignore`,
  `.p4ignore`, Plastic `ignore.conf` or the Unity root folders no longer
  reaches the agent through it. The setup screen and the docs name both
  files (#182).
- A `tools/call` whose own `arguments` is not an object is refused with
  JSON-RPC `-32602` (Invalid params) on stdio and over a legacy HTTP session,
  as the 1.0.26 notes said. Before, a legacy HTTP session got `-32603` and
  stdio got a tool result with `isError: true`. The refused shapes are a
  number, a boolean, an array and a string that is not a JSON object; the
  message names the tool and the type that was sent. Absent or `null`
  arguments still mean no arguments, and a JSON string that holds an object is
  still unwrapped, on those two transports. The stateless 2026-07-28 HTTP
  transport is unchanged: it already refused these with `-32602` and its own
  shorter message, and it also refuses `null` and a JSON string. Other
  argument errors keep their codes (#184).
- Search keeps the session's project on the hosted and HTTP transport (#180).
  With no folder pin and no local index (always the case on the gateway, which
  has no checkout on disk), a search without `project_id` queued a
  workspace-wide search ahead of the project that `init` or the
  `X-ContextStream-Project-Id` header resolved, so other projects' files
  could come back as if they were in the current checkout. Only known projects
  are candidates now. A project the read scope confirmed exists is never
  widened to the workspace when its search fails; the call returns an error
  that names the project and says the search was not widened. Team searches
  and an unconfirmed, possibly stale session project can still widen.
- A search served outside the session's project says so in the text, even in
  concise mode: `[SCOPE_WIDENED]` names where it ran, each row from another
  project is labelled `[other project <id>]`, and a row the API did not
  attribute is labelled `[project not reported]`. Result rows keep the API's
  `project_id`, and the structured result carries the notice as `scope_widened`.
- `scope_reliability.reason` no longer reports `scope_mismatch_reindexing`
  for a project whose canonical index is ready when the exact checkout cannot
  be attested (the gateway cannot read one); it reports
  `checkout_not_attested` with `scope_match: true`. A widened search reports
  `workspace_scope` or `other_project_scope`.
- Dependencies: `Cargo.lock` only, no `Cargo.toml` change. `tokio` 1.53.1 to
  1.54.1 (1.54.0 is yanked for a race in `tokio::time::Interval`,
  RUSTSEC-2026-0384; it was on `main` but in no release), `hyper` 1.11.1 to
  1.12.0, `serde_json` 1.0.151 to 1.0.154, `uuid` 1.27.0 to 1.28.0,
  `schemars` and `schemars_derive` 1.2.2 to 1.2.3, `sha2` 0.11.0 to 0.11.1,
  `libc` 0.2.190 to 0.2.191, `toml_edit` 0.25.15 to 0.25.17 with
  `toml_datetime`, `toml_parser` and `toml_writer`, and `want` 0.3.1 to 0.3.2
  (#185).
- Not in the binary: the GitHub Actions pin updates in the legacy workflows
  (#174) and a test-lock fix for the git hook tests (556546f).

## 1.0.26

- Agent writes are faster and safer to repeat. A write to plans, tasks, docs,
  memory events, decisions and nodes, lessons, todos, diagrams, reminders and
  the entity kinds (tickets, handoffs, incidents, releases, experiments,
  goals, key results, sprints, reviews, risks, backlog views, coordination
  items) now has a 10 second timeout (it was 30) and carries an
  `Idempotency-Key` that is the same on every retry of that write. A write
  that may already have reached the server (a timeout, a reset after the
  request was sent, or a 408, 500, 502 or 504 answer) is no longer retried,
  unless that same response says `x-contextstream-idempotency: 1`; a write that
  got no answer at all is retried only if the origin said so within the last
  120 seconds and no later write was answered without it.
  An answer marked `x-contextstream-idempotent-replay: 1` is never retried,
  because it only repeats the stored outcome. Connect failures before the
  request is sent, 429 and 503 are still retried, and reads keep their
  policy. Bulk event ingest and event distillation keep the general policy.
  There is no setting for the timeout or the retry rule (83eee4e, 7fc5c04,
  a471460).
- `session(action="capture_plan")` sends the plan and its tasks in one
  request, up to 100 tasks. When the API answers with the created tasks they
  are the linked tasks and no per-task call is made. A larger plan, or an API
  that does not create them, falls back to one request per task with at most 6
  in flight (`CONTEXTSTREAM_PLAN_TASK_CONCURRENCY`, 1 to 16, 1 means one at a
  time). Results and errors stay in step order, and a failed plan request is
  never followed by a second plan (83eee4e, a53a661).
- `memory(action="update_doc")` and `memory_update_doc` take `ops` instead of
  the whole document: `append`, `prepend`, `replace_section` (a heading, with
  or without its hashes; replaces the body up to the next heading of the same
  or a higher level; headings inside code fences are ignored; a missing or
  ambiguous heading is an error) and `str_replace` (exactly one match).
  `content` and `ops` cannot be sent together. The server has no precondition
  on a doc update, so a write that lands in the few milliseconds between the
  read and the update would be overwritten (83eee4e).
- A decision capture says when its conflict check did not finish. When the API
  answers before the check ran, `session(action="capture")` and
  `memory(action="create_decision")` add `[CONFLICT_CHECK_PENDING]` and set
  `operation_status.conflict_check` to `pending`; when the check was
  unavailable they add `[CONFLICT_CHECK_UNAVAILABLE]` and `unavailable`. Before
  this, both read as a capture with no conflicts (7fc5c04, a471460).
- Tools accept the argument shapes that MCP bridges send. Every tool's
  arguments are matched to its advertised schema before the handler reads
  them: an envelope key (`arguments`, `args`, `input` or `params`) that holds
  the arguments as an object or as a JSON string is unwrapped (a `tools/call`
  whose own `arguments` is not an object is still refused with `-32602`);
  camelCase and kebab-case keys move to the schema key (never over one that is
  present); `{"workspace": {"id": ...}}` and `{"project": {"id": ...}}` fill
  `workspace_id` and `project_id`; and
  values follow the property's type (booleans from `"true"`, `"false"`, 0 and
  1; integers and numbers from numeric strings; strings from scalars, from
  `{"id": ...}` or from a one-item array; arrays and objects from a JSON
  string). Ids are only moved between keys, never invented, and the API still
  checks every id. A value that still does not fit is refused as before, and
  the message now also names the tool and lists the fields whose type differs
  from the schema (it still quotes the offending value, as before). `init`
  trims `repository_url`, treats an empty one as
  absent, and sets aside one that is not a Git remote with a note and
  `repository_url_ignored` instead of failing (1bf2ede).
- `graph` reads the team's decisions as well as the code. `action="decisions"`
  with a file path or a code id as `target_id` lists the decisions, lessons and
  constraints that refer to that code, how each refers to it, then its tasks,
  notes and docs; with `node_id` it lists the related decisions.
  `action="related"` without `relation_types` shows what links to any
  knowledge node, plan, task, note, doc, ticket or handoff and the code it
  names, by distance and direction. `action="path"` shows the chain of links
  between two items of any kind (it used to read a field the API never
  returned and always answered that there was no path). The impact text adds
  "Why it is this way": the decisions, lessons and constraints about the
  target. The tool description stays within the 1,024 characters that some
  model providers accept (d8e442d, 7cd453c).
- `graph(action="call_path")` shows where a path crosses services. The text
  lists up to five paths, one numbered line each, marking an HTTP or event
  hop with its route or topic, then `Crosses services: N HTTP hop(s), M event
  hop(s).`; further paths are left to the structured result. The structured
  result and the `[WARM_CACHE]` first line are unchanged, and an API that sends
  no hops gets the plain arrow chain (580abdd).
- Local ingest understands Perforce, Plastic SCM and Unity. `.p4ignore` is
  honoured wherever `.contextignore` is, below it in precedence. In a Plastic
  workspace (a `.plastic` folder at the ingest root) the root `ignore.conf` is
  read as gitignore rules; elsewhere a file with that name is left alone.
  `.plastic` and `.vs` are never ingested. In a Unity project (a
  `ProjectSettings/ProjectVersion.txt` at the root) `Library`, `Temp`, `Logs`,
  `UserSettings`, `MemoryCaptures`, `Build`, `Builds` and `obj` are skipped
  directly under the root only, so nested folders with those names, and those
  folders in other projects, are still ingested. Files under those folders
  that an earlier version indexed are dropped by the absence check, and the
  file watcher no longer wakes for Unity's `Library` churn (580abdd).
- The hosted gateway logs and counts what it was blind to. A failed
  `tools/call` is logged under the target `contextstream::tool_errors` with the
  tool, the JSON-RPC code, an error class such as `validation:invalid_type` or
  `http:403` and, for a client that opened a session with `initialize`, its
  name and version (a stateless request logs the client as `unknown`), and
  counted in `mcp_tool_call_errors_total{tool,class}`. Each rewrite of an
  argument shape is counted in `mcp_input_shape_normalized_total{tool,kind}`
  and logged as `input_shape_normalized` with field names only. Every
  `tools/call` writes one `tool call timing` line under
  `contextstream::tool_timing` (tool, action, duration, outcome, API calls and
  their time, client name when there is a session), the execution latency
  metrics now cover the write tools, and the acceleration signal carries the
  real action. Argument values, content and error text are never logged
  (1bf2ede, 83eee4e).
- Hooks and rules ask for less text. The PreToolUse hook no longer says that
  memory, doc, plan and task writes "can take a few seconds" or asks the model
  to post progress updates around them, because those writes are synchronous;
  the notice stays for `project` `index` and `ingest_local` and for `media`
  `index`, which continue in the background. The managed editor rules and the
  `capture_plan`, `create_task`, `session` and `memory` tool text ask for short
  plan steps (a title and one to three sentences on what changes, where and how
  it is verified), short decisions and lessons, and `update_doc` ops for doc
  edits (83eee4e).
- Test fixtures no longer trip the public-boundary check. An input-shape test
  used a UUID outside the documentation fixtures that the check allows
  (83eee4e), and a graph test named a graph database in its sample text
  (6b2fe6b); the check had failed on `main` since that test was added (d8e442d).

Who gets what: the write policy, `capture_plan`, `update_doc` ops, the
conflict notices, the argument shapes and the graph text reach hosted clients
when the hosted gateway runs this version, and the graph actions need the
ContextStream API's new `/graph/knowledge` routes. An older API ignores the
idempotency header and the inline `tasks` field, so those changes degrade to
the previous behaviour there. The Perforce, Plastic and Unity ingest rules, the
write policy on a local machine, the hook change and the new rules text reach a
machine after `contextstream-mcp update` (the rules also need a rules refresh).

Dependencies: none changed. `Cargo.toml` and `Cargo.lock` are untouched since
1.0.25. Dependabot's open pull requests (#173, #174) are not part of this
release.

## 1.0.25

- Scope alerts you cannot miss. `init` and `context` now put a `scope_alerts`
  array in the structured result as well as `[SCOPE_MISMATCH]` in the text,
  because clients such as Claude Code hand the model only the structured part.
  An alert covers a folder or repository that belongs to another project,
  several projects claiming one repository, and a scope that is unresolved or
  needs setup. It is rebuilt on every call and never deduplicated, so it stays
  until the scope is fixed, and in the wire budget it is the last structured
  field to be dropped (e1c49e6).
- Working set: a session has a primary project (the one bound to the working
  folder) and related projects the same work touches. The server returns
  `working_set` (primary, related, suggested) with `init` and `context`, shown
  as a `[WORKING_SET]` line. A project joins as related when your message names
  its checkout, when an edit lands in its checkout, or with
  `project(action="working_set", add=[...])`; `remove=[...]` and `clear=true`
  take projects out. A folder that belongs to a related project is cross-project
  work, not a mismatch. The `project` tool's input schema gains `add`, `remove`
  and `clear`; every earlier field and action is still accepted (e1c49e6).
- Search covers related projects: the primary project's results come first,
  then a labelled `[RELATED_PROJECTS]` section (`related_project_results` in the
  structured result) with up to 5 results from each of up to 3 related
  projects, read within 2.5 seconds. The related list comes from a per-caller
  cache that `init` and `context` fill (5 minutes) or one bounded API read, so it
  works on the stateless hosted transport. To search one related project fully,
  pass its `project_id` (e1c49e6).
- Hooks judge scope. PostToolUse notices an edit in another linked checkout,
  records that project as related on the server and in a local per-session
  copy, and tells the agent once per project. PreToolUse checks every
  scope-sensitive ContextStream call: a project outside the working set, or
  another workspace, gets `[SCOPE_MISMATCH]` with the exact fix, reads included;
  the primary and related projects are quiet, and tickets stay quiet inside the
  workspace. This replaces the generic "Scope alignment warning", which also
  fired on deliberate cross-project writes. The managed rules and the prompt
  reminders explain primary and related projects and the two notices (e1c49e6).
- Search keeps `duplicate_count` and `duplicate_locations` in compact output.
  The API now folds identical chunks found at different paths into one result;
  when the structured payload went over its budget the compact form kept only
  an allowlist of metadata and dropped both fields. Compact output now keeps the
  count and shows up to 5 `path:start-end` locations (then "+N more"), and the
  text adds a line such as "identical chunk also in 5 other files: ...".
  Uncompacted output is unchanged (b0deb40).
- Agent Coach Kit: the `/coach` server instructions and the `workflow` and
  `workflow_save` descriptions now say when the kit applies. A host that adds
  the server by address (the pilot route) receives no skill file, so these
  texts are all it reads to decide when to use the kit, and they said what the
  kit is but not when it applies. They now say it applies when the person
  starts or continues a job for work they repeat, reacts to a draft, compares
  versions or asks what earlier jobs taught, and that a one-off request or an
  ordinary question uses none of the tools. The wording describes and does not
  command, the form ChatGPT flagged in 1.0.23, and a test keeps "do not use",
  "never use" and "recurring workflow" (which made ChatGPT create a scheduled
  task of its own) out of all four texts.
- `workflow_save` lists the parameters of `record_verdict` (the job, a verdict
  of better, same or worse, and the person's words), so a verdict on a whole
  job or version is not taken for feedback on its content.
- The text grows, unlike 1.0.24: the instructions 737 to 985 characters,
  `workflow` 1331 to 1528, `workflow_save` 2054 to 2237; `workflow_delete` is
  unchanged at 518. The tools, their parameters and their behaviour are
  unchanged.
- The plugin skill, the Codex Cloud skill and the AGENTS snippet carry the same
  boundary (repository only), and the review run sheet has the reviewer type
  the first case without choosing the plugin first, so the recording shows
  whether ChatGPT reaches for it unprompted (repository only).
- Test fixtures in the compact-search tests use neutral paths and documentation
  UUIDs (67a2f62); the public-boundary check had failed on `main` since those
  tests were added (b0deb40).

Who gets what: the scope alerts, the working set, the related-project search
and the project tool's new parameters reach hosted clients when the hosted
gateway runs this version, and need the ContextStream API that returns
`working_set` (live). The hook behaviour and the new rules text reach a machine
after `contextstream-mcp update` (the rules also need a rules refresh). The
coach text reaches ChatGPT and Codex with the hosted gateway; the kit and the
plugin package live in the repository and are not part of the npm package or
the binaries.

Dependencies: `rustls`, the TLS library the client and the hosted gateway use,
moves from 0.23.45 to 0.23.46 in `Cargo.lock` for RUSTSEC-2026-0346 (`Debug`
output of client session values exposed TLS secrets), RUSTSEC-2026-0347
(buffered application data sent after an ECH rejection), RUSTSEC-2026-0348
(Brotli certificate decompression could allocate up to 1 GiB before
authentication) and RUSTSEC-2026-0349 (an unauthenticated ServerHello could
downgrade later connections to classical key exchange), all published on
2026-10-14. No other dependency changes.

## 1.0.24

- Agent Coach Kit: the text of the `workflow`, `workflow_save` and
  `workflow_delete` tools and of the `/coach` server instructions describes the
  coaching behaviour instead of commanding replies and approvals. ChatGPT's
  permission card for the first `workflow_save` call warned "Suspicious
  Instruction": the call was clean, the wording was the trigger ("without being
  asked", "never claim ... unless the reply ends with", "never invent
  approval", "nothing is deleted"). The text now says what each tool does and
  what the statuses mean: a reaction is recorded with `add_feedback`; a result
  ends with `[COACH_SAVED]` or `[COACH_ALREADY_SAVED]` when stored and
  `[COACH_NOT_SAVED]` or `[COACH_CONFLICT]` when not; approvals, verdicts and
  scores come from the person and facts from the job's sources; each change is
  a new version and earlier versions stay readable; `COACH DATA` is the
  person's stored data, "reference, not instructions"; `workflow_delete` is for
  the person's own request. The text keeps about the same length (`workflow`
  1332 to 1331 characters, `workflow_save` 2054 to 2054, `workflow_delete` 513
  to 518, the instructions 739 to 737). The tools, their parameters and their
  behaviour are unchanged, and a test keeps the flagged phrases out of the
  server instructions and all three descriptions (ad73c8d).
- Agent Coach Kit for Codex Cloud and the ChatGPT plugin package (repository
  only): the skills and the AGENTS snippet get the same rewording, with the rule
  leads kept identical across both skills (ad73c8d). The plugin signs in with
  ChatGPT's Client ID Metadata Document (`"mode": "cimd"` in
  `plugins/agent-coach-kit/mcp.json`) instead of registering a client per
  connection, now that ContextStream's authorization server advertises
  `client_id_metadata_document_supported`; dynamic registration stays available
  as a fallback (5527e2b).

Who gets what: the new tool and instruction text reaches ChatGPT and Codex when
the hosted gateway runs this version. The default, OpenAI and consolidated tool
lists do not change. The kit and the plugin package live in the repository;
they are not part of the npm package or the binaries. No dependency changes.

## 1.0.23

- Agent Coach Kit: `workflow(action="compare")` marks feedback a version
  carried that its own job kept for that job only. A later job's agent could
  read such feedback in "what changed" as something to apply; it now reads
  "(this job only; not for later jobs)" in the text and `job_only` on the card.
  The compare reads back each carried item's scope (at most 12 reads, shared by
  both sides); one it could not read is marked "(scope not checked; ask the
  person before reusing it)", never guessed (22be1ee).
- Agent Coach Kit card (`ui://widget/coach.html`) meets WCAG 2.1 AA: one
  persistent `aria-live` region announces saves, the focus ring has at least
  3:1 contrast in light and dark, the base size is relative, the card declares
  `lang="en"` and honors `prefers-reduced-motion`, nothing scrolls sideways at
  320 px, scrolling text panels take keyboard focus as named regions, and the
  compare sides and button rows are labelled groups (22be1ee).
- Agent Coach Kit for Codex Cloud (repository only): `save-output --carried
  REF:HOW` records what each version carried, a new `verdict` command records
  the person's verdict on an open or closed job, `metrics` shows how the
  coaching is going from real jobs only (no quality score), and the skill asks
  only about what is missing or changed (5a383d4, cd77a8f).
- Agent Coach Kit package for the ChatGPT and Codex plugin directory
  (`plugins/agent-coach-kit`, repository only): the plugin listing, the one
  remote server `https://mcp.contextstream.io/coach` with OAuth (`read:coach`,
  `write:coach`), the coaching skill, and `plugins/build_agent_coach_kit.py`,
  which refuses a package that breaks a submission rule it can check. Nothing
  has been submitted (e0bfb25).

Who gets what: the compare label and the card reach ChatGPT and Codex when the
hosted gateway runs this version. The kit and the plugin package live in the
repository; they are not part of the npm package or the binaries. No
dependency changes.

## 1.0.22

- Agent Coach Kit: a new `/coach` endpoint lets a person coach an agent on
  recurring work from ChatGPT or Codex. It serves exactly three tools:
  `workflow` (read-only: `capabilities`, `list`, `get`, `context`, `history`,
  `compare`, `metrics`, `export`), `workflow_save` (append-only: create a
  workflow, refine its goal, start a job, save an output as text, an https link
  or a file the host hands over, add, rescope or retract feedback, edit or
  import guidance, set effort, close, reopen or cancel a job, record a
  verdict) and `workflow_delete` (one workflow, only with `confirm` set to its
  slug; hidden at once, erased within 30 days). Every write carries an
  operation id derived from its arguments, so a retry never saves twice, and
  ends with one status line: `[COACH_SAVED]`, `[COACH_ALREADY_SAVED]`,
  `[COACH_CONFLICT]` or `[COACH_NOT_SAVED]`. Replies name things by readable
  reference (`weekly-note#4/v2`), keep the person's words ("You said") apart
  from the coach's reading, and fence saved data as data, not instructions.
  `/coach` has its own OAuth protected-resource metadata
  (`/.well-known/oauth-protected-resource/coach`, scopes `read:coach` and
  `write:coach`) and serves a ChatGPT widget (`ui://widget/coach.html`); hosts
  without widgets get the same information as text. The coach tools stay off
  every other surface; a server started with
  `CONTEXTSTREAM_TOOL_SURFACE_PROFILE=coach` serves only them. Coach is a pilot:
  accounts without it get `coach_not_enabled`, which is not treated as a
  sign-in problem (3ccd4fe, 956e406).
- Hosted gateway: the HTTP gateway registers the coach tools (it builds its own
  tool registry, so `/coach` listed nothing there), `compare` no longer shows a
  version as revising itself, and `get` on guidance shows the person's words
  beside the coach's reading (956e406).
- Tool results can carry `_meta`, which the transports merge into the
  response's `_meta`; the coach tools use it to ask ChatGPT to sign the person
  in after a 401 or 403 (3ccd4fe).
- Agent Coach Kit for Codex Cloud (`kits/agent-coach-kit/codex-cloud`): Codex
  Cloud tasks cannot load MCP servers, so a repo skill
  (`.agents/skills/contextstream-coach/SKILL.md`) and a standard-library helper
  (`scripts/coach.py`) call the coach REST API with the person's key from a
  Codex network secret. The README covers the environment, network access, the
  storage host for large media, every command, the save tags and the exit
  codes. The kit lives in the repository; it is not part of the npm package or
  the binaries (066161c, 2809dab).

Who gets what: the coach tools reach ChatGPT and Codex when the hosted gateway
runs this version; the default, OpenAI and consolidated tool lists do not
change. No dependency changes.

## 1.0.21

- Hosted gateway: tools no longer read or write the gateway host's own disk.
  On `mcp.contextstream.io`, a tool that takes a local path acted on the
  gateway's files for any signed-in caller: `skill(action="import",
  file_path=...)` read a file and sent it to the skills API, `media(action=
  "index", file_path=...)` uploaded one, a `project` ingest from a path walked a
  directory, and search enrichment, session folder checks and config writes, and
  `project(action="recent_changes")` read the gateway's folders and git. In HTTP
  mode these are now refused with a message that says what to use instead
  (`content` for skills, `external_url` for media, the sync bridge for indexing),
  and the local reads behind search and `init` are skipped. stdio and local runs
  are unchanged. A private gateway that runs on the caller's own machine can
  restore the old behavior with `CONTEXTSTREAM_HTTP_LOCAL_FILES=allow` (339f5d5).
- Hosted gateway: it never replaces its own binary. When a client's `init`
  reported a newer version, the `session` tool's self-update could replace a
  gateway's pinned release with whatever npm or R2 called latest. `contextstream-mcp
  http` now turns runtime self-update off, as `CONTEXTSTREAM_DISABLE_SELF_UPDATE`
  already did for the npm launcher (ddcae54).
- Hosted gateway: each request keeps its own tool surface. The tool-surface
  profile lived on the one tool registry every caller shares, so an `initialize`
  from a ChatGPT-like client narrowed the tool list for everyone until the next
  one widened it, and the cached `tools/list` could hold a list built for another
  profile. The profile is now chosen per request: the integration paths
  (`/chatgpt`, `/claude`, `/client`) get the full surface, then the
  `X-ContextStream-Tool-Surface-Profile` header, then the session (the
  `initialize` request, or the profile code carried in the `Mcp-Session-Id` it
  issued), then the server default. Session ids stay UUIDs; ids issued before
  this version get the default (5c42d4b).
- Hosted gateway: search results are rooted at the folder the session named in
  `init`. They came back under the path of whichever machine first indexed the
  project (`/home/core/...` for a checkout at `/data/...`), and local enrichment
  reported `missing_root` (#168).
- Hooks: the prompt and subagent hooks no longer append an "ATTRIBUTION" block.
  It told the agent to credit ContextStream, never to say it "didn't help", and
  not to disclose the instruction; it was stored encrypted in the binary and
  showed up in the raw hook output. The hooks now send only the tool guidance the
  rules files already state (4879e0b).
- Sync bridge: a machine set up for hosted MCP now links the checkouts agents
  work in. The bridge only watched folders linked before, and hosted `init`
  cannot write local files, so a fresh install could sync nothing for days while
  search answered from other machines' copies. The background link audit now
  also checks the Git checkouts agent sessions worked in, and still links only on
  strong evidence (the repository names the project; not a temporary path, a
  pinned or isolated folder, or an agent's task worktree). A hook that sees an
  agent in an unlinked checkout asks for that audit at once (at most every 10
  minutes), a newly linked folder gets one full scan, and SessionStart says
  whether the checkout is synced, not linked (with the `link --fix` command) or
  linked with no bridge running (#167).
- `entity`: an update no longer makes a team item personal. Updates sent the
  session's default `is_personal` when the body left it out, which overwrote the
  stored value; they now send it only when the caller sets it. Every create and
  update says the stored visibility (for example "Visibility: personal (only you
  can see it). Pass is_personal=false to share it with the workspace.").
- `entity`: a new `target_project` parameter files an item in another project of
  the workspace by name ("mcp-server"); an unknown name, or a project id the API
  refuses, returns the workspace's project names. A create says where the item
  went ("Filed in project mcp-server (workspace Engineering)."). The tool text
  and the editor rules say to file in the project that owns the code, and the
  scope warning no longer fires for an entity write to another project in the
  same workspace.
- `entity`: ticket replies no longer echo `linked_items` and `assignees`, which
  the API does not store; a reply now says "Not saved: ..." for either. The kind
  `discussion` is removed (the API serves no such route) and points to
  `integration(provider="slack", action="discussions")` (901a2fc).
- Search: only skill-like queries pull in skills. Keywords matched inside other
  words, so a code search mentioning "workflow" (a CI file, a `workflow_id`
  field) was enriched with skill matches. Keywords now match whole words, and
  "workflow" counts only without CI, YAML or code words in the query (6bbf26b).
- stdio: `resume` and `resume_list` no longer offer the caller its own session.
  `init` over stdio now adopts the session id the API assigned, so the id it
  prints, the id calls are stored under and the id `resume` leaves out are the
  same; an id the caller sends, and the hosted path, are unchanged (#152, #166).
- CLI: one-shot commands end quietly when their reader closes early. `about |
  head -2` or `setup --yes | head -8` panicked and exited with 134; they now end
  on SIGPIPE like other command-line tools. The stdio server, the HTTP gateway,
  the sync bridge and the editor hooks are unchanged (#159, #166).
- Setup: the review card keeps a space after labels wider than its label column
  ("Deep project learning  No change (default)") (074c45f).
- Hooks, opt-in: with `CONTEXTSTREAM_COACHING_TURNS=1`, the Stop hook keeps the
  agent's last reply per session in `~/.contextstream/coaching-turns.json` (at
  most 200 sessions, 24 hours, 16 KiB per reply), and the prompt hook sends it
  with the next prompt to the coaching loop, alongside the context fetch. A new
  `contextstream-mcp hook codex-notify` does the same as Codex's `notify`
  program; configuring it opts in (`CONTEXTSTREAM_COACHING_TURNS=0` still turns
  it off). Nothing is sent without the opt-in, and sending pauses for a day when
  the server answers `learning_off` (053d961).

Who gets what: hook, sync bridge, setup and CLI changes reach a machine after
`contextstream-mcp update` (the editor-rules wording also needs a rules
refresh). The hosted-gateway fixes and the `entity` tool changes reach hosted
clients when the hosted gateway runs this version.

Dependencies: `quinn-proto` moves from 0.11.15 to 0.11.19 in `Cargo.lock` for
RUSTSEC-2026-0338 to RUSTSEC-2026-0342. It is an optional dependency of
reqwest's HTTP/3 support, which this build does not enable, so no released
binary contained it; the bump keeps `cargo audit` clean. No other dependency
changes (87d6488).

## 1.0.20

- Init: a placeholder id no longer fails the session. Agents started sessions
  with `init` failing `404 Workspace 00000000-0000-0000-0000-000000000000 not
  found`: the global rules files carried a placeholder `# Workspace ID:` header
  (since #145) and rule 1 told agents to pass the ids in their rules, so `init`
  took the all-zero id as a real request and skipped folder resolution. Now a
  placeholder `workspace_id` or `project_id` (the all-zero UUID in any
  spelling, an empty string, `<id>` or `{id}` templates, and `null`, `none`,
  `nil` or `undefined`) is dropped before any tool handler runs, and `init`
  reads it itself and reports what it ignored, in the text and in the
  structured `scope_repairs` list. A
  workspace that does not exist (404) is set aside and `init` runs again from the
  folder, git remote and name instead of failing; a workspace that exists but is
  forbidden is still an error, so a session never opens in another account's
  data. A project that is not in the workspace is repaired with an accurate note
  (38c2cf3).
- Init: when the folder matches nothing and the session falls back to the scope
  it was pinned to, `init` adds a `[SCOPE_RESOLUTION]` notice and a
  `scope_resolution` block in the structured result. They name the pinned
  project, up to five projects with similar names, and the exact calls to switch
  or create one. It changes nothing, and stays silent when the folder name
  matches the pinned project (38c2cf3).
- Rules: global rules files (`CLAUDE.md`, `AGENTS.md` and the other editors'
  global files, and Aider's shared file) no longer carry the `# Workspace:`,
  `# Project:` and `# Workspace ID:` lines; project rules keep their real
  workspace. Rule 1 now says never to send a placeholder id and to call
  `init(folder_path)` alone when none is known. Rules reach a machine with
  `contextstream-mcp update`; until then the runtime handling above covers the
  old header (38c2cf3).
- Delete and supersede act only on an id or an exact title. `memory(action=
  "delete_doc", doc_id="deploy notes")` deleted "Deploy runbook" when that was the
  only doc with "deploy" in its title, and the only sign was a "Resolved ..." line
  in the text. One resolver now decides for every tool that takes an id or a
  title (memory records, docs, decisions, lessons, plans, entities and skills): a
  UUID is the record's id, and other text is graded against the titles the tool
  listed (`exact_title`, `same_words`, `all_words`, `some_words`). Every delete,
  every supersede, `complete_todo`, the retiring `decision_action` values
  (`supersede`, `choose_successor`, `invalidate`), and an update that closes,
  archives, completes or abandons a record need `exact_title`; a read or an edit
  needs `all_words` or better, from exactly one record. Anything else changes
  nothing and returns the candidates, as a tool result with `isError` and the same
  list in the structured result. Two records with one exact title are both
  returned. A resolved call says which record it took, in the text and under
  `lookup_resolution` in the structured result (#162).
- Lookup, what else changed with it (#162): records match on their title only (an
  event without a title answered to its type and content, so `delete_all` on
  "decision" removed every such event on the page; tasks, todos and diagrams fell
  back to their description). Exact no longer ignores punctuation and no longer
  compares only ASCII letters and digits. A lone partial match no longer resolves
  for a read or an edit, and a tie is returned as a tie (plans took any shared
  word and broke ties by status and recency). A pending todo can be completed by
  title, and a second doc with the same title is no longer hidden by the caller's
  `limit`. No input parameter is added, removed or retyped. An unresolved
  reference used to be an error value, a JSON-RPC error on the hosted gateway; it
  is now a tool result with `isError`. The rule is stated in the parameter text
  of three tools (`action` of `memory` and `session`; `id` of `entity`; and
  `diagram_id`, `event_id`, `task_id` and `todo_id` of `memory`); their short
  descriptions are unchanged. That text reaches hosted clients when the hosted
  gateway runs this version.

## 1.0.19

- Connected apps: the `integration` tool now reaches the apps a person connects
  in ContextStream (Microsoft 365, Google Workspace, Dropbox, Box, and Slack,
  Notion, Linear and Figma), so an agent can find an item, read it as it is now
  and change it (#160). Five new actions: `apps` (what is connected, and how far
  an agent may go in each: read only, read and propose edits, or read and edit),
  `find` (one live search by name across every connected app; each hit carries
  the ids `read` takes), `read` (an item attached with `@` by `reference_id`, an
  item `find` returned by `connection_id` and `resource_id`, or a named
  `operation` with its `arguments`; the result carries the item's revision),
  `capabilities` (the operations an agent can run on one connection, with their
  arguments, and whether edits apply directly) and `edit` (prepares and applies
  a change in one call; the result says whether it was applied and confirmed, is
  waiting for approval, or was refused, and on a revision conflict the agent is
  told to read again and retry). `provider` gains `microsoft`, `google_workspace`,
  `dropbox` and `box`. The actions call the hosted API's integration runtime;
  against an older API they return its error and nothing else changes. Every
  action still needs `provider` (use `all` when no one app is meant), and
  `workspace_id` when the client keeps no state.
- Connected apps, `edit` on a client that keeps no state: an edit is recorded
  under a session, and the hosted gateway on the stateless 2026-07-28 contract
  (Claude Code) has none stored between calls, so `edit` could only refuse and
  `init` could not help. `integration` now accepts `session_id`, the id `init`
  returned (its `resume_hint` names it). Without one the id the API assigned at
  `init` is used, then the server's local id, and the refusal says to pass
  `session_id` (#163). `apps`, `find`, `read` and `capabilities` need no session.
- The `integration` tool's description is a routing summary under 1,024
  characters, and the full action reference moved to the `action` parameter, as
  for `capsule`, `entity` and `memory` (#160). The schema change is additive. The
  tool text reaches hosted clients when the hosted gateway runs this version.
- Hooks: text is cut on a character boundary. The SessionEnd, PreCompact,
  SubagentStop and SubagentStart handlers cut text at a byte count (a tool
  result at 2000 bytes, a subagent summary at 4000, a Plan subagent's title at
  120 and description at 12000, a decision preview at 200) and panicked when an
  emoji, a CJK character or an accented letter straddled the cut, which lost the
  capture. ASCII output is unchanged; other text is cut at the start of the
  straddling character (#161). Two similar byte slices outside the hooks, an
  HTTP error detail in `setup/profile.rs` and API key masking in `setup/mod.rs`,
  are not changed here. The fix takes effect on a machine that runs this version
  of the binary.
- Dependencies: `uuid` 1.26.1 to 1.27.0 and `libc` 0.2.189 to 0.2.190, in
  `Cargo.lock` only; `Cargo.toml` is unchanged (#155). The `attest-build-provenance`
  pin in the legacy release workflow moved to 4.2.2 (#156); releases are not
  built by that workflow.

## 1.0.18

- Resume: agents are now told how to resume, with their own session id. A
  resume that is given the id `init` returned skips the caller's own session, but
  nothing told an agent to pass it. The `session` tool's description now says to
  use `resume` or `resume_list` with the id `init` returned, its action reference
  documents both actions, `session_id` says it is that id for `resume` and
  `resume_list`, and the unknown-action error lists them. The rules every editor's
  file carries map "resume", "resume recent work", "pick up where we left off"
  and "continue my last session" to `session(action="resume" | "resume_list",
  session_id=<id init returned>)`, no longer to `recall`; the rules reach a machine
  with `contextstream-mcp update`. The tool text reaches users when the hosted
  gateway runs this version (29a1bbd).
- Hooks: in Claude Code and Codex, a prompt that asks to resume earlier work
  gets the exact call, the latest session or `resume_list` when the user wants to
  choose. Only specific phrases count, prompts over 600 characters are ignored,
  and "update my resume", "resume.pdf" and "resume the download" do nothing.
  PostToolUse records the session id an `init` result names, per host session
  (the host id is hashed, kept for 24 hours), so the hook still names the right id
  after a long conversation or a compaction. Cursor and Windsurf get the guidance
  from the tool text and the rules only (29a1bbd).
- Git events: the pre-push hook also sends the commits a push carries, as
  `pushed_commits` (newest first, at most 50, shas only), so every commit in a
  pushed range shows as pushed and not only its tip. It claims nothing when git
  cannot name the range, and the hosted API ignores the field if it does not know
  it (63cfbf1).
- Git events are tagged with the session id `init` returned, for Claude Code,
  Codex and Cursor. They were tagged with the host's own session id, which no
  transcript or resume card uses, so a push shortly after a commit was dropped from
  its card. With no `init` on record nothing is tagged, and the host id is never
  sent. After a commit the hook sends a copy of the event that names the session
  and agent, and the backend keeps one row. Cursor's `afterShellExecution` also
  runs `git-bash-observed` (refresh Cursor's hooks to get it), and Codex events say
  `codex` or `cursor` instead of always `claude_code`. Codex gets this only where it
  already runs the ContextStream Bash hook, because setup installs no Codex hooks
  (8f637f6).

## 1.0.17

- Claude Code auto memory: setup now turns Claude's auto memory off when it
  configures Claude Code, and the hooks deny writes to Claude's auto memory files
  (`~/.claude/projects/*/memory`) with a message naming the ContextStream call to
  use instead (`session(action="remember")`, `capture_lesson`, or a `fact` node).
  To keep auto memory on, answer so in the setup wizard's review step, run
  `setup --keep-claude-auto-memory`, or set `CONTEXTSTREAM_KEEP_CLAUDE_AUTO_MEMORY=1`;
  `CONTEXTSTREAM_ALLOW_LOCAL_MEMORY=true` lifts the write denial. `doctor` reports
  the setting and `doctor --repair --scope global` turns auto memory off.
  `import-local-memory` copies existing memory files into ContextStream once
  (a63d160).
- Hooks: an approved Claude Code plan is saved as a ContextStream plan with one
  linked task per numbered step. A plan with the same title is adopted, and
  approving again updates it. Repository runbooks, ADRs, RFCs, postmortems and
  notes are mirrored to ContextStream docs keyed by repository and path, for
  checkouts linked to a workspace (scrubbed, 256 KB cap). Codex `apply_patch`
  writes get the same handling. Hooks recognize every ContextStream server name,
  including the claude.ai connector. Save guidance is injected once per prompt and
  only when the prompt asks to save something, and it routes each kind
  (preference, lesson, decision, doc, plan, todo, ticket, handoff). The
  `on-save-intent` hook is no longer installed (a63d160).
- Hooks: plan capture and doc mirroring use the workspace and project that a
  successful `init` or `context` resolved for the host session (kept for 24
  hours), so they also work in checkouts that only have a legacy folder link. A
  repository doc written before the session has a scope is queued and mirrored
  once `init` or `context` succeeds. `import-local-memory` skips folders with no
  workspace link, explains a 403 on a stale link, and takes `--project-dir`,
  `--workspace-id` and `--project-id` to import one Claude project into an
  explicit scope (fb1bac8).
- Search-first nudges give the exact ContextStream call for each local search:
  regex to `mode=pattern`, literals to `exhaustive`, identifiers to `refactor`,
  file-name searches to `pattern` with `output_format="paths"`, `-C/-A/-B` to
  `context_lines`, glob and type filters to `file_types`, and Explore to
  `mode=guided` with the task as `intent`. The same table now reaches Explore and
  Plan subagents. The rules recipe for goal, risk and term nodes passes `title`
  and `content`, which `memory(action="create_node")` requires (a63d160, ffab454).
- Project links: `contextstream-mcp link [--path|--all] [--fix] [--project-id]
  [--pin] [--json]` shows which project a checkout is linked to, ranks the
  candidates with the reasons, and re-links with a verified repository
  fingerprint. A checkout is ranked by the Git repository it tracks, other
  checkouts of the same repository on the machine, names, and where it was
  indexed before; benchmark, isolated, generated, worktree-named, dated or
  temporary projects are penalized, and a close call is reported as ambiguous.
  The sync bridge runs the same audit two minutes after it starts and every six
  hours, repairs only links backed by the repository or a sibling checkout, never
  into an isolated workspace and never for a pinned or temporary folder, and
  records every change with the previous link in `~/.contextstream/link-audit.json`
  (`CONTEXTSTREAM_LINK_AUDIT=0` turns the audit off). `doctor --scope project`
  reports a `project_link` check and `doctor --repair` applies a confident fix.
  `project(action="ingest_local")` reuses the folder's existing project instead of
  creating a new one named after the folder, and worktrees no longer start new
  projects (452bd2a).
- Session: the session id and the `resume` call that 1.0.16 added to `init`, and
  the own-session note on `resume` and `resume_list`, were only in the text of
  those results, which Claude Code does not show. They are now in the structured
  result too: `init` adds `resume_hint` next to `session_id`, and `resume` and
  `resume_list` write the finished text, with the note and the scope note, into
  the structured text. The text itself is unchanged (3241462).
- `testing/adoption` adds `persistence_adoption.py`, which measures the share of
  an agent's durable saves that went to local files, and `search_adoption.py` now
  reads Codex code-mode `exec` calls. `scripts/ovh-cargo.sh` defaults to
  ovh-desktop (a63d160, ffab454).

## 1.0.16

- Session: `resume` and `resume_list` can now leave out your own session on the
  hosted gateway for clients that connect with the stateless 2026-07-28 protocol,
  such as Claude Code 2.1.289. On that protocol the gateway keeps no state
  between calls, so the session id that `init` stored (the 1.0.15 approach) did
  not exist when `resume` ran, and `resume` returned the session `init` had just
  opened. The caller now carries the id: `init` prints "Session id: <id>" and the
  `resume` call to make, and `resume` and `resume_list` add a one-line note naming
  that call when no id was available. The stored-id fallbacks stay for stdio and
  for initialize-era HTTP, where state persists. The `resume_id` parameter text
  now says to pass the session id `init` returned.

## 1.0.15

- Session: `resume` and `resume_list` now leave out the session `init` just
  opened. The 1.0.14 exclusion did not take effect when the caller passed no
  `session_id`: the server kept a random local id, while the API stores snapshots
  under the id it returns from `init`, which was never read. `init` now records
  the API's id, and `resume` and `resume_list` exclude, in order, the explicit
  `session_id`, the API's id, the id the server was initialized with, then the
  transport's MCP session id.

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
