# Data handling and controls

This document describes the open-source client's default network behavior. The
setup wizard prints the same material before it writes configuration.

## Defaults

| Flow | Default | Data sent | Control |
|---|---:|---|---|
| Deep project learning | off for new accounts | Context-use and feedback signals in the hosted account, only after separate consent | Account → Privacy; `contextstream-mcp configure --account-learning status` checks state, `--account-learning on` (alias `--deep-learning on`) opens browser consent or prints the URL, and `--account-learning off` withdraws. Interactive `setup` remembers a decline per account locally under `~/.contextstream/account-learning.json`; plain `setup --yes` preserves the current choice |
| MCP transcript exchange saving | on | User/assistant exchanges supplied to `context` when transcript saving applies | `CONTEXTSTREAM_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --transcripts off` |
| Hook transcript saving | on | Supported editor lifecycle exchange payloads | `CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --hook-transcripts off` |
| Project indexing | setup-dependent | Files matched after ignore rules, plus index metadata | Skip indexing; exclude paths with gitignore-style rules in `.contextstream/ignore` (relative to the project root) or in a `.contextignore` in any folder (files excluded after indexing are dropped on the next complete index; local search fallbacks skip them too, and so does the local snapshot refresh of hosted hits, so a checkout's rules also hide hits that another checkout of the same project uploaded earlier); use `project(action="purge")` to de-index server-side content |
| Editor lifecycle hooks | selected supported editors | Hook-specific MCP requests documented by generated rules | `CONTEXTSTREAM_HOOK_ENABLED=false` or uninstall/update the managed hooks |
| Mid-run coordination notices | on where hooks are installed | While a session makes tool calls, at most every 35 s a detached background request asks for new coordination notices: session id, workspace/project scope, folder path, agent handle (`CONTEXTSTREAM_AGENT_NAME` or an `agent-<name>` scratch-folder name), and on the first request a 500-character task summary. Without hooks, the local MCP server makes the same request and adds notices to the next ContextStream tool result | `CONTEXTSTREAM_MID_RUN_NOTICES=0`; interval `CONTEXTSTREAM_NOTICE_POLL_SECS` (5–300) |
| Usage metadata | on | A random installation ID stored in `~/.contextstream/installation.json` (never derived from a hostname, username, path or hardware), client name and version, OS family and version, CPU architecture, the editors setup configured, and workspace, project and session IDs. They go with `init`, and in two events posted to `/api/v1/activation/events`: `first_mcp_action_succeeded` (once per session; tool name and operation category) and `mcp_connection_failed` (stage, error code, HTTP status). The metadata carries no file contents, queries, tool arguments or paths | `CONTEXTSTREAM_ACTIVATION_EVENTS=off` turns off both the `init` fields and the events; the original `CONTEXTSTREAM_ACTIVATION_SCHEMA_VERSION=0` still works |
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

- `project(action="purge")` removes indexed project content while retaining the
  project record.
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
