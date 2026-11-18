# Data handling and controls

This document describes the open-source client's default network behavior. The
setup wizard prints the same material before it writes configuration.

## Defaults

| Flow | Default | Data sent | Control |
|---|---:|---|---|
| Deep project learning | off for new accounts | Context-use and feedback signals in the hosted account, only after separate consent | Account → Privacy; `contextstream-mcp configure --account-learning status` checks state, `--account-learning on` (alias `--deep-learning on`) opens browser consent or prints the URL, and `--account-learning off` withdraws. Interactive `setup` remembers a decline per account locally under `~/.contextstream/account-learning.json`; plain `setup --yes` preserves the current choice |
| MCP transcript exchange saving | on | User/assistant exchanges supplied to `context` when transcript saving applies | `CONTEXTSTREAM_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --transcripts off` |
| Hook transcript saving | on | Supported editor lifecycle exchange payloads | `CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --hook-transcripts off` |
| Project indexing | setup-dependent | Files matched after ignore rules, plus index metadata: with each batch, the checkout's credential-free repository identity and fingerprint, branch, commit SHA and time, whether the tree is dirty, and random installation and machine IDs (never a hostname or local path) | Skip indexing; exclude paths with gitignore-style rules in `.contextstream/ignore` (relative to the project root) or in a `.contextignore` in any folder (files excluded after indexing are dropped on the next complete index; local search fallbacks skip them too, and so does the local snapshot refresh of hosted hits, so a checkout's rules also hide hits that another checkout of the same project uploaded earlier); files over 5 MiB are never indexed (`index` and `setup` say how many were left out and name the first few; `.contextignore` is not needed for them); use `delete(action="project.purge", project_id=...)` to de-index server-side content (it cannot be undone, so it runs only through the `delete` tool, after the person confirms) |
| Checkout snapshot | off until a tool that the API defines for your account asks for one (none is defined for any account today); never on the hosted gateway | What your checkout holds on top of its last commit, as git sees it: the commit's tree id; for each changed, added or deleted path its name, mode and content hash; and the checkout's folder path, so the tool's output names your files. The content of the changed and added files goes with the request, in a git pack; a file of the last commit goes only when the tool's service says it lacks it, and nothing else is ever given out. The repository is named by its folder name and a SHA-256 of its `origin` URL or, with no `origin`, of the ids of its first commits (before its first commit, of its folder path); the name holds the digest only, never the URL, the ids or the path. All three are digests that someone who can guess the input can confirm, and the folder path can include your home folder's name. Left out: everything `.gitignore` leaves out, `node_modules` folders, Cargo's `target` folder, files listed in `.contextstream/ignore` or a `.contextignore`, and uncommitted files that look like credentials (a secret file name such as `.env`, or a private key or service-account key inside); the snapshot lists the paths it withheld. Nothing is written to your repository | `CONTEXTSTREAM_CHECKOUT_SNAPSHOT=off`; `"checkout_snapshot": false` in `.contextstream/config.json` (commit it to turn snapshots off for everyone in the project); `CONTEXTSTREAM_CHECKOUT_SNAPSHOT_REPOS` limits snapshots to repositories whose `origin` URL or folder name starts with one of the comma-separated prefixes |
| Tools the API defines for your account | the list is read when a session starts; a tool is called only when you or your assistant call it (none is defined for any account today); never on the hosted gateway | Reading the list sends what every API request carries, plus the workspace ID. A call sends the tool's arguments as your assistant gave them and the workspace ID; a tool whose definition asks for a checkout snapshot also sends what the Checkout snapshot row describes. To a tool that answers over a stream, the call itself carries only the repository's folder name and a digest of its `origin` URL or, with no `origin`, of its first commits' ids or its folder path (never the URL, the ids or the path), and the rest goes over the stream; the call is made while the rest is being prepared. When the API answers a call with a stream, this server opens it (a `wss` address the API names; redirects are not followed) with a token that is valid for that one call, never your API key, and sends: the run's description from the API (an id, the command or arguments the tool defines, a folder inside the repository, time limits and a repository key), the checkout snapshot with the content of the changed files, answers to the service's requests for the last commit's file list or file contents, and the values of the environment variables the API names (never one whose name looks like a credential, nor a value that holds a login in a URL), and a description of your local environment, so the service can tell whether it can reproduce it: the operating system and CPU architecture names, the CPU count, whether the process runs as root, the time zone's name, and the first line that `<tool> --version` prints for each tool the run's description from the API names (at most 16). A tool is found only in the folders of your own `PATH` that are outside the repository, never in the repository or the command's folder, and is run directly, without a shell, in the command's folder, for at most two seconds, with its installers told not to install anything; a version manager that picks the version by folder may therefore run, and one that installs a missing version on first use may begin to (the time limit cuts it short). Nothing else about your variables, paths or files is part of that description; a tool's version line is cleaned first: words in it that look like paths (they contain a slash or a backslash, or start with `~`) are left out, your user name, your home folder's name and your host name are replaced with `<redacted>` (names of fewer than three characters excepted), and what is left is cut to 200 characters. A command sent by `run` is also marked as one whose output is taken when it ends, so the service may hold the output until it has decided the result. What the service answers is returned to your assistant, and its heartbeats show as progress notifications. When the service does not run a call, your assistant is told to run it itself | `CONTEXTSTREAM_HOSTED_TOOLS=off` reads no list and offers no such tool; `CONTEXTSTREAM_HOSTED_ENV=off` sends no description of your environment (the service then treats it as unknown) and asks no tool for its version; the Checkout snapshot switches stop what a call sends of your checkout (the call is then not made) |
| Command routing (Claude Code) | off until a tool that the API defines for your account lists the shell commands it takes (none does for any account today); never on the hosted gateway | The server keeps that tool's name and the command patterns it lists in `~/.contextstream/hosted-command-hints-<account digest>.json` (no credential; tied to your account and API address; used for 12 hours). When Claude Code is about to run a shell command that one of the patterns matches as a whole, the editor hook changes the call so that your shell runs `contextstream-mcp run --tool <name> -- '<your command>'` instead, and Claude Code shows that changed command. The hook grants no permission: Claude Code asks about, allows or refuses the changed command as it would any other, and an allow rule you wrote for the original command does not cover it. `run` sends the command to the tool, with the descriptions of your checkout and your environment that the previous row lists, prints what comes back as it arrives, and ends with the command's own exit status after one line that says it ran remotely. When no tool takes the command, the tool declines, or it cannot be reached, `run` runs your command right there, in your folder, with `bash -c`, as you typed it (bash, as the host and ContextCode run it; where bash is not installed, `run` says so and runs nothing). A command no pattern matches is never changed, and patterns that would take everyday commands (`ls`, `git status`, `pwd`, `cat README.md`) are ignored | `CONTEXTSTREAM_HOSTED_TOOLS=off` (no hints are kept, the hook changes nothing and `run` runs everything here); `CONTEXTSTREAM_HOOK_ENABLED=false` or removing the managed Claude Code hook; the Checkout snapshot switches stop what is sent of your checkout |
| Editor lifecycle hooks | selected supported editors | Hook-specific MCP requests documented by generated rules | `CONTEXTSTREAM_HOOK_ENABLED=false` or uninstall/update the managed hooks |
| Mid-run coordination notices | on where hooks are installed | While a session makes tool calls, at most every 35 s a detached background request asks for new coordination notices: session id, workspace/project scope, folder path, agent handle (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name), and on the first request a 500-character task summary. Without hooks, the local MCP server makes the same request and adds notices to the next ContextStream tool result | `CONTEXTSTREAM_MID_RUN_NOTICES=0`; interval `CONTEXTSTREAM_NOTICE_POLL_SECS` (5–300) |
| Live run progress | sent when your assistant calls the `progress` tool; where hooks are installed and you turned it on (`CONTEXTSTREAM_RUN_PROGRESS_AUTO=on`; off by default), also the signals in [Live run progress from the hooks](#live-run-progress-from-the-hooks) | The report as your assistant wrote it: a title, the step it is on, its checklist, a percentage, links it opened (PR, preview, deploy), what it waits on and a final summary; the workspace and project, and the ticket, loop, plan and step ids the report names; a run key (on the local server, a random id made for the MCP process plus the ticket id; on the hosted gateway, a digest of the session or caller plus the ticket id; or the key your assistant gave); the agent: the name you configured (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name), else the name the MCP client gives itself with four characters of the session part ("Claude Code 3fa9") and an id made of the client name and eight of them, so two sessions are two agents; never a folder, user or host name. Workspace members who can read the project see it live. After a report, the local MCP server sends a background request every 30 s with only the workspace, project, run key and agent (no step and none of your assistant's words), so the run is shown as live while the session is open; it stops when the run is reported finished, when the MCP process exits, or after four hours without a report. The hosted gateway sends no background request, except in the workspaces its operator lists in `CONTEXTSTREAM_RUN_PROGRESS_SIGNS_WORKSPACES` (none by default): there a tool call of a session it can tell apart (by the session header or `Mcp-Session-Id`, never by a name alone) counts as a sign of life for that session's own default run, the one its reports that name no run and no ticket go to. At most once a minute, under your own key or sign-in, the gateway reads that run and, when it is open, not waiting and reported on within four hours, sends a bare report with only the workspace, the run id and the run's agent. It does so only once the API has said whose that key or sign-in is: the gateway asks the API for the account (`GET /auth/me`, under your own key or sign-in) when you connect, and again after a report of yours was recorded under a key or sign-in it has no such answer for, at most once a minute. No other answer of the API counts, and a refusal ends it. The gateway remembers the answer as two digests (of the key or token, and of the account id), never the key, the token or the id: for ten minutes, or, for a key or sign-in a run was reported under, for as long as that run can get a sign (four hours). Signs are counted by that account: sixty a minute in a workspace, whatever keys and sessions it uses. Runs reported under your own `run_key` or on a ticket get no such sign | `CONTEXTSTREAM_RUN_PROGRESS=off` (the tool then answers that reporting is off and sends nothing) |
| What a person sends a run | asked for where the `progress` tool or the hooks know a run reported under an API key; answered only where ContextStream delivers answers to the workspace (nowhere in production today: the API answers 404, and is then left alone for ten minutes) | See [What a person sends a run](#what-a-person-sends-a-run): a read with the workspace id and the run id; for what waits, a claim with the run id and the agent's labels; and, when your assistant confirms it, the note it wrote and the claim's token | `CONTEXTSTREAM_RUN_PROGRESS=off` (no run is reported, so nothing is asked), `CONTEXTSTREAM_MID_RUN_NOTICES=0` (the hooks do not ask) |
| Usage metadata | on | A random installation ID stored in `~/.contextstream/installation.json` (never derived from a hostname, username, path or hardware), plus the client name and version, OS family and version, and CPU architecture, go in `X-ContextStream-*` headers on the server's API requests; `init` adds the editors setup configured. Two events posted to `/api/v1/activation/events` add workspace, project and session IDs: `first_mcp_action_succeeded` (once per session; tool name and operation category) and `mcp_connection_failed` (stage, error code, HTTP status). The metadata carries no file contents, queries, tool arguments or paths | `CONTEXTSTREAM_ACTIVATION_EVENTS=off` turns off the headers, the `init` fields and the events; the original `CONTEXTSTREAM_ACTIVATION_SCHEMA_VERSION=0` still works |
| Session header (Claude Code, hosted server) | on where setup wrote Claude Code's hosted entry; the agent name only where you also turned on signs of life (`CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on`; off by default) | One header, `X-ContextStream-Client-Session`, with a random id that `contextstream-mcp hook mcp-headers` prints when Claude Code connects (its `headersHelper`). Never derived from a hostname, username, path, key or the editor's session id. With signs of life on, also `X-ContextStream-Agent-Name` with your agent's name (`CONTEXTSTREAM_AGENT_NAME`, else `AGENT_NAME`, else the name in an `agent-<name>` working folder), on every request of that session whatever the workspace. See [The session header](#the-session-header-claude-code) | `CONTEXTSTREAM_RUN_PROGRESS=off` (the command then prints no header); without `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on` no name is sent |
| Local Git capture | on | Event type; commit SHA/time; branch/ref names; aggregate additions, deletions and files changed; first-line commit subject bounded to 256 characters and redacted; opaque `checkout-v1:<uuid>`; canonical credential-free repository remote; optional session/agent label | `CONTEXTSTREAM_GIT_CAPTURE=off`, or set `.contextstream/config.json` `git_capture.enabled=false`; restrict `git_capture.events` |

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
  line always come from the local tool.
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
