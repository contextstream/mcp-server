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
| Live run progress | sent only when your assistant calls the `progress` tool | The report as your assistant wrote it: a title, the step it is on, its checklist, a percentage, links it opened (PR, preview, deploy), what it waits on and a final summary; the workspace and project, and the ticket, loop, plan and step ids the report names; a run key (on the local server, a random id made for the MCP process plus the ticket id; on the hosted gateway, a digest of the session or caller plus the ticket id; or the key your assistant gave); the agent name (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name) and the name the MCP client gives itself. Workspace members who can read the project see it live. After a report, the local MCP server sends a background request every 30 s with only the workspace, project, run key and agent (no step and none of your assistant's words), so the run is shown as live while the session is open; it stops when the run is reported finished, when the MCP process exits, or after four hours without a report. The hosted gateway sends no background request | `CONTEXTSTREAM_RUN_PROGRESS=off` (the tool then answers that reporting is off and sends nothing) |
| Usage metadata | on | A random installation ID stored in `~/.contextstream/installation.json` (never derived from a hostname, username, path or hardware), plus the client name and version, OS family and version, and CPU architecture, go in `X-ContextStream-*` headers on the server's API requests; `init` adds the editors setup configured. Two events posted to `/api/v1/activation/events` add workspace, project and session IDs: `first_mcp_action_succeeded` (once per session; tool name and operation category) and `mcp_connection_failed` (stage, error code, HTTP status). The metadata carries no file contents, queries, tool arguments or paths | `CONTEXTSTREAM_ACTIVATION_EVENTS=off` turns off the headers, the `init` fields and the events; the original `CONTEXTSTREAM_ACTIVATION_SCHEMA_VERSION=0` still works |
| Local Git capture | on | Event type; commit SHA/time; branch/ref names; aggregate additions, deletions and files changed; first-line commit subject bounded to 256 characters and redacted; opaque `checkout-v1:<uuid>`; canonical credential-free repository remote; optional session/agent label | `CONTEXTSTREAM_GIT_CAPTURE=off`, or set `.contextstream/config.json` `git_capture.enabled=false`; restrict `git_capture.events` |

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
