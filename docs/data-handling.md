# Data handling and controls

This document describes the open-source client's network behavior. The setup
wizard summarizes its data controls before writing configuration.

## Defaults

| Flow | Default | Data sent | Control |
|---|---:|---|---|
| Deep project learning | off for new accounts | Context-use and feedback signals in the hosted account, only after separate consent | Account → Privacy; `contextstream-mcp configure --account-learning status` checks state, `--account-learning on` (alias `--deep-learning on`) opens browser consent or prints the URL, and `--account-learning off` withdraws. Interactive `setup` remembers a decline per account locally under `~/.contextstream/account-learning.json`; plain `setup --yes` preserves the current choice |
| MCP transcript exchange saving | on | User/assistant exchanges supplied to `context` when transcript saving applies | `CONTEXTSTREAM_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --transcripts off` |
| Hook transcript saving | on | Supported editor lifecycle exchange payloads | `CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --hook-transcripts off` |
| Project indexing | setup-dependent | Files matched after ignore rules, plus index metadata: with each batch, the checkout's credential-free repository identity and fingerprint, branch, commit SHA and time, whether the tree is dirty, and random installation and machine IDs (never a hostname or local path) | Skip indexing; exclude paths with gitignore-style rules in `.contextstream/ignore` (relative to the project root) or in a `.contextignore` in any folder (files excluded after indexing are dropped on the next complete index; local search fallbacks skip them too, and so does the local snapshot refresh of hosted hits, so a checkout's rules also hide hits that another checkout of the same project uploaded earlier); files over 5 MiB are never indexed (`index` and `setup` say how many were left out and name the first few; `.contextignore` is not needed for them); use `delete(action="project.purge", project_id=...)` to de-index server-side content (it cannot be undone, so it runs only through the `delete` tool, after the person confirms) |
| Checkout snapshot | Sent when a local hosted check opens a stream, including automatic preparation and opted-in speculative runs | Tree IDs, changed or deleted paths, changed file contents and the local checkout root. The worker can ask for the base tree's listing and files. See [Checkout snapshots](#checkout-snapshots) | `CONTEXTSTREAM_CHECKOUT_SNAPSHOT=off`, `"checkout_snapshot": false` in `.contextstream/config.json`, or a prefix allowlist in `CONTEXTSTREAM_CHECKOUT_SNAPSHOT_REPOS` |
| Tools the API defines for your account | Available according to the account's settings. `check` runs when called or when an enabled hook schedules work | Tool arguments and workspace scope. A streamed tool can also send the checkout and the environment described below. See [Tool calls and environment](#tool-calls-and-environment) | `CONTEXTSTREAM_HOSTED_TOOLS=off` or turn off remote checks for your account. `CONTEXTSTREAM_HOSTED_ENV=off` omits the environment description and version probes |
| Command routing (Claude Code) | With the managed hook and a tool whose command hints match | The hook passes the command to `contextstream-mcp run`, which sends it with the normal hosted-check payload when a tool takes it. See [Tool calls and environment](#tool-calls-and-environment) | `CONTEXTSTREAM_HOSTED_TOOLS=off` or `CONTEXTSTREAM_HOOK_ENABLED=false` |
| Preparation (`:prepare`) | Background call from supported local hooks when `check` is available | Repository identity, workspace scope when known, detected language IDs and the normal snapshot stream | `CONTEXTSTREAM_HOSTED_TOOLS=off` or disable checkout snapshots |
| Speculative checks | Off until opted in | Changed paths for planning and the normal snapshot stream for each admitted check | `CONTEXTSTREAM_SPECULATIVE_CHECKS=off` overrides the account setting. The hosted-tool and snapshot switches also apply |
| Contract checks | On for eligible hosted MCP checks | Changed path names and bounded, filtered file contents for consumer checks | `CONTEXTSTREAM_CONTRACT_CHECK=off` skips the consumer check. The primary check keeps its normal payload |
| Verification gates | Off until set in workspace or project policy | Scope for the policy lookup. An enabled gate also sends the gate name, target tree ID and repository key when known | `GET` and `PUT /api/v1/mcp/hosted-tools/gate-policy`. `CONTEXTSTREAM_HOSTED_TOOLS=off` disables the local receipt lookups |
| Verification maps | Read when the project is planned or the map is opened | Planning sends paths and scope. Saving a map sends its directory rules and revision | Edit the project's Verification Map in Capsules or through the API |
| Editor lifecycle hooks | selected supported editors | Hook-specific MCP requests documented by generated rules | `CONTEXTSTREAM_HOOK_ENABLED=false` or uninstall/update the managed hooks |
| Mid-run coordination notices | on where hooks are installed | While a session makes tool calls, at most every 35 s a detached background request asks for new coordination notices: session id, workspace/project scope, folder path, agent handle (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name), and on the first request a 500-character task summary. Without hooks, the local MCP server makes the same request and adds notices to the next ContextStream tool result | `CONTEXTSTREAM_MID_RUN_NOTICES=0`; interval `CONTEXTSTREAM_NOTICE_POLL_SECS` (5–300) |
| Live run progress | sent when your assistant calls the `progress` tool; where hooks are installed and you turned it on (`CONTEXTSTREAM_RUN_PROGRESS_AUTO=on`; off by default), also the signals in [Live run progress from the hooks](#live-run-progress-from-the-hooks) | The report as your assistant wrote it: a title, the step it is on, its checklist, a percentage, links it opened (PR, preview, deploy), what it waits on and a final summary; the workspace and project, and the ticket, loop, plan and step ids the report names; a run key (on the local server, a random id made for the MCP process plus the ticket id; on the hosted gateway, a digest of the session or caller plus the ticket id; or the key your assistant gave); the agent: the name you configured (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name), else the name the MCP client gives itself with four characters of the session part ("Claude Code 3fa9") and an id made of the client name and eight of them, so two sessions are two agents; never a folder, user or host name. Workspace members who can read the project see it live. After a report, the local MCP server sends a background request every 30 s with only the workspace, project, run key and agent (no step and none of your assistant's words), so the run is shown as live while the session is open; it stops when the run is reported finished, when the MCP process exits, or after four hours without a report. The hosted gateway sends no background request, except in the workspaces its operator lists in `CONTEXTSTREAM_RUN_PROGRESS_SIGNS_WORKSPACES` (none by default): there a tool call of a session it can tell apart (by the session header or `Mcp-Session-Id`, never by a name alone) counts as a sign of life for that session's own default run, the one its reports that name no run and no ticket go to. At most once a minute, under your own key or sign-in, the gateway reads that run and, when it is open, not waiting and reported on within four hours, sends a bare report with only the workspace, the run id and the run's agent. It does so only once the API has said whose that key or sign-in is: the gateway asks the API for the account (`GET /auth/me`, under your own key or sign-in) when you connect, and again after a report of yours was recorded under a key or sign-in it has no such answer for, at most once a minute. No other answer of the API counts, and a refusal ends it. The gateway remembers the answer as two digests (of the key or token, and of the account id), never the key, the token or the id: for ten minutes, or, for a key or sign-in a run was reported under, for as long as that run can get a sign (four hours). Signs are counted by that account: sixty a minute in a workspace, whatever keys and sessions it uses. Runs reported under your own `run_key` or on a ticket get no such sign | `CONTEXTSTREAM_RUN_PROGRESS=off` (the tool then answers that reporting is off and sends nothing) |
| What a person sends a run | asked for where the `progress` tool or the hooks know a run reported under an API key; answered only where ContextStream delivers answers to the workspace (nowhere in production today: the API answers 404, and is then left alone for ten minutes) | See [What a person sends a run](#what-a-person-sends-a-run): a read with the workspace id and the run id; for what waits, a claim with the run id and the agent's labels; and, when your assistant confirms it, the note it wrote and the claim's token | `CONTEXTSTREAM_RUN_PROGRESS=off` (no run is reported, so nothing is asked), `CONTEXTSTREAM_MID_RUN_NOTICES=0` (the hooks do not ask) |
| Usage metadata | on | A random installation ID stored in `~/.contextstream/installation.json` (never derived from a hostname, username, path or hardware), plus the client name and version, OS family and version, and CPU architecture, go in `X-ContextStream-*` headers on the server's API requests; `init` adds the editors setup configured. Two events posted to `/api/v1/activation/events` add workspace, project and session IDs: `first_mcp_action_succeeded` (once per session; tool name and operation category) and `mcp_connection_failed` (stage, error code, HTTP status). The metadata carries no file contents, queries, tool arguments or paths | `CONTEXTSTREAM_ACTIVATION_EVENTS=off` turns off the headers, the `init` fields and the events; the original `CONTEXTSTREAM_ACTIVATION_SCHEMA_VERSION=0` still works |
| Session header (Claude Code, hosted server) | on where setup wrote Claude Code's hosted entry; the agent name only where you also turned on signs of life (`CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on`; off by default) | One header, `X-ContextStream-Client-Session`, with a random id that `contextstream-mcp hook mcp-headers` prints when Claude Code connects (its `headersHelper`). Never derived from a hostname, username, path, key or the editor's session id. With signs of life on, also `X-ContextStream-Agent-Name` with your agent's name (`CONTEXTSTREAM_AGENT_NAME`, else `AGENT_NAME`, else the name in an `agent-<name>` working folder), on every request of that session whatever the workspace. See [The session header](#the-session-header-claude-code) | `CONTEXTSTREAM_RUN_PROGRESS=off` (the command then prints no header); without `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on` no name is sent |
| Local Git capture | on | Event type; commit SHA/time; branch/ref names; aggregate additions, deletions and files changed; first-line commit subject bounded to 256 characters and redacted; opaque `checkout-v1:<uuid>`; canonical credential-free repository remote; optional session/agent label | `CONTEXTSTREAM_GIT_CAPTURE=off`, or set `.contextstream/config.json` `git_capture.enabled=false`; restrict `git_capture.events` |

## Remote checks and verification

### Checkout snapshots

The initial HTTP request identifies the repository by its checkout folder name
and a SHA-256 digest of its `origin` URL. Without an origin, the input to the
digest is the sorted IDs of the repository's initial commits, or the resolved
checkout path before the first commit. The request sends the digest and folder
name. Someone who can guess the input can confirm it against the digest.

If the API issues a stream, the stream sends the last commit's tree ID, the
snapshot tree ID, the absolute checkout root (which can include your username),
the repository-relative paths, Git modes and blob IDs of changed and added
files, and deleted path names. It sends a Git pack with the changed and added
blobs, including a changed symlink's target text. The worker can request the
base tree's full file listing and blobs from that tree or the changed set. The
client refuses other objects and builds the snapshot without changing your
Git index or object store. The hosted MCP gateway cannot take a snapshot of
your machine.

The change pack excludes Git-ignored untracked files and skips untracked
files in generated directories such as `node_modules` and a cache-tagged
Cargo `target` directory. Tracked changes in those directories can still
be sent. The client skips changed files excluded by `.contextstream/ignore`
or a `.contextignore`.
For changed regular files, the client also checks credential filenames and
looks for private keys, service-account keys and AWS access key pairs in the
first 256 KiB of files up to 1 MiB. Those filters apply to the changed-file
pack. The worker can still request a withheld path's committed version from
the base tree, and deleted path names are sent separately. Withheld-file
reasons stay local on the streamed `check` path. A tool that takes an HTTP
snapshot attachment receives the repository identity, both tree IDs, change
counts and withheld path names and reasons.

`CONTEXTSTREAM_CHECKOUT_SNAPSHOT=off` stops snapshots. You can also commit
`"checkout_snapshot": false` in `.contextstream/config.json`, or restrict
eligible repositories with `CONTEXTSTREAM_CHECKOUT_SNAPSHOT_REPOS`.

### Tool calls and environment

The tool catalog request carries the workspace ID when known. The initial
call sends the tool arguments, workspace scope when known, and the repository
identity for a streamed check. The stream uses a token for that call. Your API
key authenticates the HTTP request. The stream also carries the run details
from the API, including the command, repository key, working directory within
the repository and time limits.

For `check`, `run`, `:prepare` and speculative checks, the stream can include
values for environment variables the API requests. The client filters
credential-like names and values with URL logins and forwards up to 32 values
of up to 4 KiB each. If a variable the API marks as local-only is set, the client
declines to open the stream after the initial request. The variable's value
stays local. By default the stream also includes the OS and CPU architecture,
CPU count, whether the process runs as root, the time zone and up to 32 tool
versions. `CONTEXTSTREAM_HOSTED_ENV=off` removes that description and skips
the probes. Forwarding requested environment variables uses its own filter.

The client probes a tool found on `PATH` outside the repository, running it
directly in the command's folder for up to two seconds with install-suppression
settings. It uses `go version` for Go, `zig version` for Zig and tries
`java --version` followed by `java -version` for Java. Most tools use
`--version`. It skips banner lines, removes path-like words apart from
Go's trailing OS/architecture, redacts the user, home folder and host names
when they are at least three characters long, and keeps up to 200 characters
per version line. A version manager can still pick a version based on the
folder and may try to install a missing version within the time limit.

In Claude Code, the managed hook can route a matching shell command through
`contextstream-mcp run`. It uses command hints cached locally by credential
digest and API address and leaves shell permissions to the client. If the
remote check declines or is unavailable, `run` runs the command locally.
For a supported failing check with structured failure details, the client
can send a follow-up to `/api/v1/mcp/hosted-tools/calls/<call_id>/diagnose`
with those details, the base comparison if available, the time budget and
any known workspace, project and repository key. The text report can cite
matching context. The hosted service can turn repeated failures or a failure
followed by a passing check into a workspace lesson, using scrubbed check
metadata already received by the service.

### Preparation and speculative checks

A supported session-start hook or post-edit hook can start `:prepare` in the
background when remote checks are available. The HTTP call sends the repository
identity, workspace ID when known, the `:prepare` request and language IDs
inferred from local marker filenames. If the API issues a stream, the client
sends a checkout snapshot with the current changes and the usual environment
fields described above. The worker can fetch missing base tree data over
that stream, materialize the checkout and prepare dependencies. There is no
check command from your session to run. Preparation costs no credits and
failures are silent.

Opted-in speculative checks start after a successful edit reported by the
post-tool hook and a debounce period. The snapshot is of the checkout at that
point, so it can include other uncommitted changes. When a workspace is
configured, the planner receives its ID, the project ID when configured, the
repository key, up to 128 repository-relative changed or deleted paths from
the snapshot, and targeted scope. The client sends no file contents to the
planner. A background check admitted to run sends the selected command,
working directory when applicable, speculative flag and credit limit, local
opt-in when used, repository identity and workspace scope. Each check then
uses the normal snapshot and environment stream. The local credit limits
control execution. The planning request can still happen when those limits
have been reached.

### Contract checks

After a successful hosted MCP `check` with recorded changes and a workspace,
the client can make a second request to
`POST /api/v1/mcp/hosted-tools/contract-check`. The request carries the workspace
ID, the session's project ID when known, the checkout folder name and origin
digest, and up to 200 distinct repository-relative paths from the snapshot.
This list includes changed, added and deleted paths. The API can receive
those names even if their contents are ineligible.

For selected changed and added files, the request also carries the path,
Git mode, SHA-256 digest and full UTF-8 contents of the blob from that
snapshot, encoded as base64. The client sends at most 24 files, up to 64 KiB
each and 512 KiB of decoded file bytes in total. Selection covers OpenAPI
and Swagger documents, protobuf, GraphQL, Avro, SQL and TypeScript declaration
files, and paths under `contracts/`, `openapi/`, `swagger/`, `api/`, `routes/`,
`handlers/`, `schema/`, `schemas/`, `types/`, `sdk/` and `proto/`. When the
snapshot has at most 24 added or changed files, ordinary `.ts`, `.tsx`, `.rs`,
`.go` and `.py` files are also candidates.

The client supplies contents only for eligible regular UTF-8 files. It
skips symlinks, snapshot exclusions, oversized files and recognized secret
filenames such as `.env` variants other than standard templates, `id_rsa`,
`.npmrc`, `credentials.json` and key files. It scans the full text of eligible
blobs for private-key blocks, service-account JSON containing a private key,
and AWS access key pairs. A match withholds the entire blob. Other embedded
values can still be present in the bytes. The primary check's snapshot has
already been sent according to the ordinary snapshot rules.

ContextStream looks up consumers in the same workspace using project graph
links and matching indexed contract paths, and uses each consumer's GitHub
connection to resolve its default branch. The MCP can open up to four
consumer streams. Each stream sends the API's run details, the consumer's
tree ID, and the contract bytes the API mapped to that consumer's paths.
The client sends an empty Git pack and no local environment values or tool
versions. If the worker needs consumer base files, the client ends the stream
without supplying them. The worker applies the change to a temporary copy.
The client reports a consumer as unverified when the worker cannot show
it applied the overlay. A consumer check that runs is billed as a normal
check, subject to the usual cache and billing rules. The secondary request
is skipped for automatic preparation, speculative checks and shell `run`.

### Receipts and gate checks

The hosted service can record a passing receipt from a verified, signed
usage report for a remote run that exited successfully. The receipt includes the
call, workspace and actor IDs, repository key, target tree ID, command digest,
command family or engine, exit code and verification time. It is created by
the service from the hosted run. The client makes no separate receipt upload.
A receipt for the primary check is about that run. Consumer contract outcomes
come back separately after it. Use required check families when the policy
needs proof that particular checks ran. The receipt lookup accepts any fresh
passing receipt for the tree when no families are required.

Local gate lookups send the workspace and project IDs when known to
`GET /api/v1/mcp/hosted-tools/gate-policy`. The policy can be cached on disk
for 45 seconds. When it enables the gate, a second request to
`GET /api/v1/mcp/hosted-tools/receipts/check` adds the gate name, exact Git
tree ID and repository key when available. Git's `pre-push` hook uses the trees
of the refs Git is sending. The editor's pre-tool gate uses the checkout tree,
computed locally with the snapshot exclusions. The requests carry no file
contents, checkout path, shell command text or task text. A push can also send
its ordinary metadata when Git capture is enabled. The local hook lets the
action proceed if the policy or receipt lookup fails.

### Verification maps

For targeted `verify`, the planning request carries the workspace ID, any
resolved project ID and repository digest, the normalized repository-relative
changed paths and the scope. Those paths can come from the input, a local
snapshot or Git status and diff. The planning request carries path names even
when snapshot uploads are disabled. If the planner returns a trace ID, the
client sends it with subsequent hosted checks so ContextStream can tie their
outcomes to the plan. Each remote check still sends its regular check payload.

The service can store edited directory names, planned commands and working
directories in a project trace and link eligible hosted calls to it.
Suggestions come from your recent traced checks that found regressions.
Opening the map reads stored rules by workspace and project. Saving it sends
those IDs, the revision and the complete rule list of directory, command
and working directory to `PUT /api/v1/mcp/hosted-tools/verification-map/<project>`.
The web app transfers no local file contents when it saves a map. A project's
Live Capsule can show its verification rules.

### Capsule and handoff summaries

ContextStream builds verification summaries for capsules and handoffs from
stored receipts and check metadata. These can include tree and commit IDs,
command families, status, known base failure counts and timestamps. Reading
the summary causes no checkout upload. A handoff can compare a target tree
stored in its metadata.

Creating a Review Capsule from a local MCP session is a separate, explicit
upload. The client sends the repository name, commit range, review text and
selected Git diff hunks or excerpts after local secret filtering, with the
workspace and project scope. The review can carry receipt information from
the hosted service.

## Live run progress from the hooks

Off by default. You turn it on for yourself with `CONTEXTSTREAM_RUN_PROGRESS_AUTO=on` in the environment your editor
starts with. Until you do, the hooks write no state, start no process and send nothing, and live progress is only what
your assistant reports with the `progress` tool.

With it on, where ContextStream's hooks are installed, they keep a session's live runs honest without your assistant
having to remember. Each signal is one `POST /api/v1/run-progress/report` made with your own key, the route the `progress` tool
uses. Workspace members who can read the run's project see it live, on the ticket and wherever runs are shown.

| When | What is sent |
|---|---|
| A tool call of the session sets a ticket to `in_progress` (`entity(kind="ticket", ...)`), and `init` on a 1.0.77 or later server (local, or the hosted gateway) has said who the session is | A run starts: workspace and project, the ticket's id and its own title, status `working`, a run key (the server's session part plus the ticket id, the key the `progress` tool uses), the agent, and a group id (the session agent's id). No step, and nothing your assistant wrote |
| A tool call of the session's own work (not one of ContextStream's) starts or ends two minutes or more into a turn, `init` on a 1.0.77 or later server has said who the session is, and the session has had no run: your assistant reported none and took up no ticket | The session's own run starts: the workspace and project `init` resolved (else the ones the folder is linked to, and the folder's project where `init` resolved a workspace and no project in it; without a workspace, nothing), the title "Working in" followed by the project's name as the server gave it (at most 80 bytes; "Working" alone without one), status `working`, a run key (the server's session part plus `session`, the key a `progress` report that names no run uses), the agent, and a group id. No step, and nothing your assistant wrote. At most once per MCP connection. This and the row above are the two cases that report a session whose assistant never called `progress` |
| Your assistant reports with `progress` after that | A report that names no run lands on the session's own run, which is your assistant's from then on (its title stays: the API sets a run's title when the run starts). A report on a ticket or under a `run_key`, or a ticket taken up, is another run: the session's own run then ends with status `stopped` and the sentence "The session reported its work as another run." |
| A run is open and the session's process is alive | Every 30 s: workspace, project, run, agent and group id, nothing else. Every 10 minutes instead while the run waits on a person. The project is the run's own. For a run your assistant reported without one, the next of these signals (within seconds) offers the project `init` resolved for the session, else the one the folder is linked to, and only when that project is in the run's workspace: the API then shows the run in that project. If the API refuses it (a project your key may not write), it is not offered again and the run is kept alive as it was reported |
| Your assistant's turn ends | Status `waiting_input` with `waiting: {"on": "person"}`. No question text |
| You send a prompt, or the assistant calls a tool, after that | Status `working` |
| A tool call starts ten minutes or more after the last step your assistant reported, or at any time on the session's own run while your assistant has reported nothing on it | One of seven fixed sentences, picked by the tool's name alone and sent at most once a minute: "Reading files", "Searching the code", "Editing files", "Running a command", "Reading the web", "Working with a subagent", "Using a tool". Never a file name, a command, prompt text or an MCP server's name |
| The session ends, a subagent ends, the session's process is gone, the session sets the ticket to another status, or the MCP connection was replaced | Status `stopped` and one fixed sentence, such as "The session ended without a final report." The hooks never report `completed` and never write a summary of the work |

- **The agent** is the one the `progress` tool reports for the session (see the table above), so a run keeps one name.
  A subagent's own run is named after its session and the subagent's type ("Claude Code 3fa9 · Explore"), with an id
  that holds a digest of the subagent's id.
- **Never sent by the hooks:** prompt text, tool inputs or outputs, file names or contents, commands, a folder, user or
  host name, or the editor's own session id.
- **Local state:** `~/.contextstream/run-progress/<digest of the editor's session id>.json` holds the session's open
  runs and the signals not sent yet. Files untouched for a week are removed.
- **The keep-alive process:** tool-call hooks never touch the network. While a session has an open run, one detached
  `contextstream-mcp hook run-progress-beat` process sends its signals: one small request every 30 s for each open run
  (every 10 minutes while it waits on a person). Each session has its own; two sessions never share one. It looks every
  2 s for the session's own process and leaves within 30 s of that process ending, after ending the session's runs. It
  also leaves when the session ends, a minute after the last run closed, when the API refuses, or when the API cannot
  be reached five times in a row.
- **Codex:** Codex runs a hook only after you approve it, and as shipped setup writes one hook for it, the mid-run
  notices before a tool call. Where `CONTEXTSTREAM_RUN_PROGRESS_AUTO=on` (or `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on`) is
  set in the environment `contextstream-mcp setup` or `update-hooks` runs in, setup also writes
  `contextstream-mcp hook run-progress` under `PostToolUse` and `Stop` in `~/.codex/hooks.json`. Approve both with
  `/hooks` in Codex CLI and reload the session: Codex skips an unapproved hook without a word, so setup says which
  ones wait on every update and `doctor` warns until Codex has them. That hook does live progress and nothing else:
  it reads the answers of `init`, `progress` and a ticket's status change, notes that a tool call or a turn ended,
  prints nothing and never touches the network. The mid-run notices hook notes that a tool call starts; its command
  is unchanged, so its approval stands. What is sent is the table above, with two differences: after a turn ended, a
  Codex run says `working` again with the next tool call (not with the prompt), and a Codex session's end is told
  from its process ending. With both switches off the next `update-hooks` takes the two hooks out, and `uninstall`
  always does; your own Codex hooks are never touched. Until you turn it on and approve them, a Codex session gets
  none of this section, as before.
- **Windows:** the session's process cannot be watched there. A session that crashes is shown as stalled and later as
  lost, not as ended, and the keep-alive process leaves 10 minutes after the last hook event (60 minutes when a tool
  call never reported back).

Controls, per machine (set them in the environment your editor starts with):

- `CONTEXTSTREAM_RUN_PROGRESS_AUTO=on`: the hooks report as this section says. Unset or `off`, which is the default,
  they report nothing: no run starts by itself, and a run your assistant reports is not kept alive, shown as waiting
  between turns or ended by the hooks.
- `CONTEXTSTREAM_RUN_PROGRESS_SESSION_AFTER_SECS` (120 when unset; 0 to 3600): with the automatic reports on, how many
  seconds into a turn a session works with no run before the hooks start its own. The two minutes leave your assistant
  time to report first, under a title of its own. It changes nothing while the automatic reports are off.
- `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on` (off by default): with the automatic reports off, the hooks send only signs
  of life for the runs your assistant reported with `progress`, known by the run id its answer named: after a real
  hook event (a tool call starts or ends, a prompt arrives), at most once a minute per run, a request with only the
  workspace, the run id and the run's agent. Nothing for a run that waits, that its agent has not reported on for
  four hours, that a subagent reported last once that subagent stopped, or that a session reported under a session
  part that ended (the MCP connection was replaced); no run is started, set waiting, stepped or ended. The same
  switch sends the agent's name in the session header ([The session header](#the-session-header-claude-code)).
- `CONTEXTSTREAM_RUN_PROGRESS=off`: live progress is off altogether, whatever the switch above says. Nothing is sent,
  no state is written and no process is started, and the `progress` tool also answers that reporting is off.
- An API that answers 401, 403 or 404 to a report ends reporting for that session.
- There is no workspace-wide or per-person setting yet. It needs a change to the ContextStream API and is tracked
  there; until it exists, the two switches above are the controls.

## What a person sends a run

When your assistant asks a person something through ContextStream (a `progress` report that waits on a person, an
approval or a sign-in), the person can answer there, and can write to a running assistant. ContextStream keeps what
they sent as a delivery for the run, and this client fetches it with your own key. The API offers this only to
workspaces it is switched on for; for every other workspace the paths answer 404, as a path that does not exist.

| When | What is sent |
|---|---|
| The `progress` tool recorded a nonterminal report under a full API key, or read a run this session reported | One `GET /api/v1/decision-requests/deliveries` with the workspace id and the run id. On a 404, or a 503 `deliveries_not_ready`, the workspace is not asked again for ten minutes |
| Something waits for the run | `POST .../deliveries/<id>/claim` with the run id, the agent labels and a random attempt id as the claimant’s agent id. Only a response naming that attempt can be shown. The background read cannot claim. A report or an explicit get takes the claim inline and returns the words. The claim's answer carries a token the session keeps in memory |
| Your assistant confirms it (`progress(action="reply", ...)`) | `POST .../deliveries/<id>/applied` with the note your assistant wrote, which the person reads with the run, and the claim's token |
| Your assistant declines it (`progress(action="decline", ...)`) | `POST .../deliveries/<id>/declined` with `moot` or `cannot_apply`, its note, and the claim's token |
| Hooks are installed, the session reported a run, and it makes tool calls | The mid-run poll (at most every 35 s, a detached request) makes the same read for each run the session reported, at most eight. It claims nothing; it tells your assistant in one line that something waits and how to read it |
| Your assistant started `contextstream-mcp await-answer` before it ended its turn | The same read every 20 s (every 2 minutes after a 404 or a 503 `deliveries_not_ready`, and after failures up to every 5 minutes) for at most four hours. It claims nothing: it ends with one notice that something waits, and your assistant reads it with `progress` |

- **What a delivery can do:** it is shown to your assistant as a person's words, with the sentence that it is to be
  acted on only within the task and the permissions the session already has. This client approves no tool call,
  changes no permission mode and trusts no secret or host because a delivery says so; the editor's own permission
  prompts decide as before.
- **One claimer:** only the session's own `progress` call claims, under the credential that reported the run, and
  it confirms with the claim's token. The token is kept in the server process's memory and is never shown to your
  assistant, written to a file or logged. When a token is lost, recovery requires a private local record of the credential, session,
  workspace, run, delivery and original claim attempt. Recovery first reads the run’s delivery list. A missing delivery or a newer claim is not reclaimed. When the list shows the same holder, recovery can request its token again; the current API omits active leases from this list. For a pending delivery with no intervening claim, recovery reuses the recorded attempt and omits the run id from the claim so it cannot rotate the token of an agent that took it after the read. The client checks the returned attempt and claim count before confirming. A replica without local evidence sends nothing.
- **Only a person's full API key receives.** A session signed in with OAuth, any other token that is not an API
  key, and a key limited to one workspace (`cbiq_ws_`) ask for nothing: ContextStream raises no request for a run
  reported under them and takes no message for it, because no answer can come back to them. The `progress` tool
  reads no delivery for such a session and never tells it to start `await-answer`.
- **The tool frames each delivery.** Every line of a person's words is quoted. When the API supplies a
  wrapped message, only its quoted body lines are used. The surrounding instructions and confirmation
  line always come from the local tool. When a report or a get of the session’s own run shows a delivery,
  the result text includes its complete `[PERSON ANSWER]` or `[PERSON MESSAGE]` block. In
  `structuredContent`, each `deliveries` entry carries its `id`, `kind` and the identical block in `frame`.
  The tool omits `deliveries` when it shows none.
- **Never sent:** the answer or message itself is not sent anywhere by this client. Nothing of the folder, the prompt
  or other tool calls goes with these requests.
- **Local state:** a session's hooks keep the ids of the runs it reported and of the deliveries it was pointed at in
  `~/.contextstream/mid-run-notices/<digest>.json`, removed a day after its last use. The `progress` tool remembers
  up to 512 tokens per delivery owner in memory for at most one hour. Its bounded local binding file stores ids, digests,
  timestamps and whether a claimed delivery was shown yet, never tokens or words (in the platform state directory under `contextstream/deliveries`, or
  `CONTEXTSTREAM_DELIVERY_STATE_DIR`). The HTTP gateway requires this directory to be set explicitly. Replicas can use a private shared directory supporting file locks and atomic rename. `await-answer` keeps an empty per-run lock file under `contextstream/waiters` in the user's state directory so only one local waiter polls; its lock is released on exit.
- **Gateway memory limits:** Each gateway process keeps at most 32 delivery owners per credential across its
  workspaces and sessions, and 256 owners in total. Only a claim in flight or an unshown delivery pins the
  owner's slot, an unshown delivery for at most five minutes after its claim returns; its age is checked on
  every admission, so no sweep is waited for. Claim tokens never pin: the private binding recovers them. One
  credential pins at most eight owners at a time; past that, its new owners and its owners about to claim are
  paused, and no other credential's slot is touched. At 32, a credential can replace only its own least
  recently used unpinned owner. At 256, a credential below its own limit can take a slot from the credential
  with the most slots among those with an unpinned owner. All slots count toward that total. The gateway
  evicts that credential's least recently used unpinned owner; if credentials tie, it compares their unpinned
  owners together. An owner with only reports is eligible for eviction immediately; its idle report evidence
  expires after 10 minutes. A later report still reads and claims inline for the session's own run: its
  default or ticket run, whose key the gateway derives from the caller and the session, or a run its
  ownership record on disk names.

  When a session is paused, the gateway still records progress. The report or `get` says
  “Deliveries are paused for this session; read again later.” The gateway logs one warning with an owner
  digest per refusal. If your session has never held a slot, report again to establish ownership.

  The wait is bounded. Each delivery request times out after five seconds and a claim is sent at most
  twice, so a claim in flight ends within about ten seconds, and its unshown delivery stops pinning five
  minutes later. With no new claims, every pin ends within about five minutes and ten seconds of the last
  claim. A credential's new claims extend only its own pause; another credential's session is paused only
  while at least 32 credentials each hold eight pins.

  An unshown delivery whose owner was evicted is neither lost nor shown twice. Its API claim stays, and the
  private binding marks it unshown; it stores no message text or token. When a read lists the claim again,
  the session claims it under the same attempt, and only the response that clears the mark shows it, also
  across replicas that share the binding directory. The current API omits active claims from its list, so
  the delivery comes back when the claim's lease lapses, within an hour, and is then shown once.
- **Controls:** `CONTEXTSTREAM_RUN_PROGRESS=off` (nothing is reported, so the tool asks for nothing),
  `CONTEXTSTREAM_MID_RUN_NOTICES=0` (the hooks do not ask). `await-answer` runs only when it is started.

## The session header (Claude Code)

Claude Code speaks the stateless form of MCP: it tells a hosted server nothing about which of your sessions a request
belongs to. Up to 1.0.77 every Claude Code session of one API key was therefore one agent in live progress, on every
machine. From 1.0.78 setup adds one line to Claude Code's hosted ContextStream entry (`~/.claude.json` and a project's
`.mcp.json`): `"headersHelper": "<path>/contextstream-mcp hook mcp-headers"`. Claude Code runs that command each time
it connects and adds the header it prints to the entry's own.

- **What is sent:** one header, `X-ContextStream-Client-Session`, holding 32 random hexadecimal characters. Nothing of
  the machine, the user, the folder, the key or the editor's own session id is in it.
- **The agent's name, only where you turned it on:** with `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on` in the environment
  Claude Code starts with (off by default), the command also prints `X-ContextStream-Agent-Name` with the agent name
  the session is given by the rule a local server uses: `CONTEXTSTREAM_AGENT_NAME`, else `AGENT_NAME`, else `<name>`
  when the working folder is called `agent-<name>`. Claude Code then sends it on every request of that session to the
  hosted server, whatever the workspace. The hosted server uses it only as the agent of the session's live progress
  reports (all your sessions that send one name report as that one agent); it takes the name in its canonical form
  (lowercase letters, digits, `.`, `_` and `-`, at most 64 characters) and ignores a value that has none. It is not
  used to tell your sessions apart and gives no access. Without the switch, no name is sent and nothing changes.
- **What it is used for:** the hosted server digests it together with your key into the session part of a live
  progress report (the agent's id and name, "Claude Code 3fa9", and the default run key). It is never used without
  the key, so a value chosen under one key cannot name a session of another, and a value of any other shape is
  ignored. A client on the stateful form of MCP is named the same way by its `Mcp-Session-Id`, also digested
  together with the key. When you are signed in instead of using a key, a digest of the account your access token
  names takes the key's place (of the whole token, when it names none), so two people who send the same value are
  two sessions. The token, the account id and the key are never part of a report and are not kept. A folder a
  hosted session named (`agent-<name>`) names its agent only under a key: when you are signed in, the agent is
  named by the header above or by the session, never by the folder.
- **Local state:** Claude Code names the session's own process to the command (`CLAUDE_CODE_MESSAGING_SOCKET`). The
  id made for that process is kept in `~/.contextstream/mcp-connections/`, one file of 32 characters per session
  process, removed a week after its last use, so a session that reconnects keeps its agent. Where the process is not
  named (Windows), nothing is kept and a reconnect shows as another agent.
- **If the command cannot run:** Claude Code connects with the entry's own headers, as before, and the session is
  named by its key as in 1.0.77. The command reads no network and ends itself after two seconds at the latest.
- **A project's `.mcp.json`:** Claude Code runs a helper from a project file only in a folder whose trust dialog you
  accepted. In a folder that was never trusted (a headless `claude -p` run in a fresh checkout) it does not run the
  command, says so in one line on standard error, and connects with the entry's own headers.
- **Controls:** with `CONTEXTSTREAM_RUN_PROGRESS=off` in the environment Claude Code starts with, the command prints
  no header and keeps nothing. Setup and update leave a `headersHelper` of your own alone. An existing install gets
  the line with `contextstream-mcp update`; until then nothing changes for it.
- Other editors' entries are unchanged. Not run on Windows.

## VCS minimization boundary

The VCS request serializer rejects filesystem paths and only accepts a valid
opaque checkout ID in the legacy `repo_path` field. Configured Git remotes are
normalized to a canonical HTTPS identity with usernames, passwords, query
strings, fragments, and transport-specific user information removed. Commit
author name/email and full commit bodies are never serialized. Free-text
subjects are first-line only, control-character stripped, redacted for obvious
paths, emails, URLs, and secret assignments, and bounded to 256 characters.

These rules are enforced twice: when the validated checkout binding is converted
to an event and again immediately before the HTTP request is built.

## Local state

The client stores credentials and editor configuration on the user's machine.
The npm launcher stores only the exact-version binary and verification metadata
in the user cache. Cache files are not transcript or project content.

## Deletion and unbinding

- `delete(action="project.purge", project_id=...)` removes indexed project content
  while retaining the project record. It cannot be undone, so `project(action="purge")`
  refuses and points here: it runs only through the `delete` tool, after the person
  confirms.
- `project(action="forget_local")` removes the machine's local mapping without
  deleting server-side data.
- Remove managed MCP/rules/hook entries with the setup uninstall/cleanup flow.
- Remove the npm binary cache by deleting only the ContextStream MCP cache
  directory shown by your platform's cache conventions.

Hosted-service retention, account deletion, and subprocessors are governed by
the current ContextStream privacy documentation and account controls.

## Optional deep project learning

Your signed-in dashboard shows the current server consent text, with a checkbox
that starts unchecked. The CLI cannot grant consent and the MCP tools expose no
consent mutation. `configure` accepts `--account-learning [status|on|off]`
(alias `--deep-learning`), and `setup` accepts `--account-learning on|off`:
`status` (or bare `--account-learning`) shows your current state; `on` opens
Account → Privacy (`https://contextstream.io/dashboard/account/privacy`) in a
browser when available—or prints the URL on headless machines—unless consent is
already active for the current terms; `off` withdraws using your saved
credentials. Interactive `setup` offers the consent prompt once for first-time
users (and once per updated consent version when `requires_reconsent` is set),
storing a per-account dismissal locally in `~/.contextstream/account-learning.json`
when you say no so later setup and update runs stay quiet. Dry-run changes
neither the account nor browser state.

Collection stops immediately on withdrawal. Learned history and generated
learning items are queued for deletion in live systems within 30 days. Ordinary
content you saved remains. Learned data is never sold or used to train AI models.
Learning events and signals have a 180-day retention window; consent audit records
document your choice. A reconnect does not opt you in or reset your choice.
