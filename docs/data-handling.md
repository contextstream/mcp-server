# Data handling and controls

This document describes the open-source client's default network behavior. The
setup wizard prints the same material before it writes configuration.

## Defaults

| Flow | Default | Data sent | Control |
|---|---:|---|---|
| MCP transcript exchange saving | on | User/assistant exchanges supplied to `context` when transcript saving applies | `CONTEXTSTREAM_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --transcripts off` |
| Hook transcript saving | on | Supported editor lifecycle exchange payloads | `CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED=false` or `contextstream-mcp configure --hook-transcripts off` |
| Project indexing | setup-dependent | Files matched after ignore rules, plus index metadata | Skip indexing; add `.contextstream/ignore`; use `project(action="purge")` to de-index server-side content |
| Editor lifecycle hooks | selected supported editors | Hook-specific MCP requests documented by generated rules | `CONTEXTSTREAM_HOOK_ENABLED=false` or uninstall/update the managed hooks |
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
