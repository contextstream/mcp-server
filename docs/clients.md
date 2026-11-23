# Supported clients

`contextstream-mcp setup` detects the clients installed on your machine, writes
each one's MCP config and project rules, installs hooks where the client has
them, and checks the result with `doctor`. To choose clients yourself, pass
their ids:

```bash
contextstream-mcp setup --editors=kimi,qwen,zed
```

<!-- BEGIN GENERATED: contextstream-mcp clients --format markdown -->
| Client | `--editors` id | MCP config | Project config | Project rules | Hooks |
|---|---|---|---|---|---|
| Claude Code | `claude` | `~/.claude.json` | `.mcp.json` | `CLAUDE.md` | yes |
| Cursor | `cursor` | `~/.cursor/mcp.json` | `.cursor/mcp.json` | `.cursor/rules/contextstream.mdc` | yes |
| Windsurf | `windsurf` | `~/.codeium/windsurf/mcp_config.json` | — | `.windsurf/rules/contextstream.md` | yes |
| GitHub Copilot (VS Code) | `copilot` | `<VS Code user dir>/mcp.json` | `.vscode/mcp.json` | `.github/copilot-instructions.md` | — |
| Cline (VS Code) | `cline` | `<VS Code user dir>/settings.json` | — | `.clinerules` | yes |
| Kilo Code | `kilo` | `~/.config/kilo/kilo.jsonc` | `kilo.jsonc` | `.kilo/rules/contextstream.md` | — |
| Roo Code (VS Code) — discontinued; use Cline (VS Code) | `roo` | `<VS Code user dir>/settings.json` | `.roo/mcp.json` | `.roo/rules/contextstream.md` | yes |
| OpenAI Codex CLI | `codex` | `~/.codex/config.toml` | — | `AGENTS.md` | — |
| Aider | `aider` | rules only | — | `.aider.conf.yml` | — |
| Antigravity | `antigravity` | `~/.gemini/config/mcp_config.json` | `.agents/mcp_config.json` | `GEMINI.md` | — |
| OpenCode CLI | `opencode` | `~/.config/opencode/opencode.json` | `opencode.json` | `AGENTS.md` | — |
| Muse Code | `muse` | `~/.config/muse/settings.json` | — | `AGENTS.md` | — |
| Kimi Code CLI | `kimi` | `~/.kimi-code/mcp.json` | `.kimi-code/mcp.json` | `AGENTS.md` | — |
| ZCode | `zcode` | `~/.zcode/cli/config.json` | `.zcode/config.json` | `AGENTS.md` | — |
| Qwen Code | `qwen` | `~/.qwen/settings.json` | `.qwen/settings.json` | `QWEN.md` | — |
| Gemini CLI | `gemini` | `~/.gemini/settings.json` | `.gemini/settings.json` | `GEMINI.md` | — |
| Zed | `zed` | `~/.config/zed/settings.json` | `.zed/settings.json` | `AGENTS.md` | — |
| Claude Desktop | `claude-desktop` | `<Claude config dir>/claude_desktop_config.json` | — | — | — |
| GitHub Copilot CLI | `copilot-cli` | `~/.copilot/mcp-config.json` | — | `AGENTS.md` | — |
| Factory Droid | `droid` | `~/.factory/mcp.json` | `.factory/mcp.json` | `AGENTS.md` | — |
| Amp | `amp` | `~/.config/amp/settings.json` | — | `AGENTS.md` | — |
| Crush | `crush` | `~/.config/crush/crush.json` | — | `AGENTS.md` | — |
<!-- END GENERATED -->

For Antigravity (the desktop app and `agy`), setup also installs a ContextStream
plugin in `~/.gemini/config/plugins/contextstream`. It holds compact always-on
rules (`rules/AGENTS.md`), the full reference as the `contextstream` skill, and
hooks that remind the agent to call `init`/`context` and to search ContextStream
before its own grep and find. Antigravity shows it under its plugins
(`agy plugin list`); `doctor` reports whether it is installed and on.

Setup writes the full ContextStream rules into each client's global rules file
(for example `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md` or `~/.gemini/GEMINI.md`),
which the client loads in every session. Claude Code, Codex and Gemini CLI load
that file beside a project's own rules file, so for them the project's file holds
only the project's workspace and project, and a session in the project reads the
rules once. The project's file keeps the full rules when a client on the machine
that reads it would not get them from a global file: another client sharing
`AGENTS.md`, or `CLAUDE_CONFIG_DIR` or `CODEX_HOME` pointing elsewhere. If your
project's rules file is committed and teammates use it without running
ContextStream setup themselves, run `setup` or `update-rules` with
`CONTEXTSTREAM_PROJECT_RULES=full` to keep the full rules in it.

A machine's rules stay where they are. When the global rules file of a client
set up on the machine holds no ContextStream block (someone took it out, so the
project files carry the rules), `update`, `update-rules` and `setup` leave it
that way and keep the full rules in the project's file, rather than loading them
twice. To move them back into the global file, run
`CONTEXTSTREAM_PROJECT_RULES=header contextstream-mcp update-rules --scope=all`
in each set-up project. Each rules write says which form it gave a project's
file and why. `update` run outside a set-up project refreshes only the global
files, and says so.

`<VS Code user dir>` is `~/.config/Code/User` on Linux,
`~/Library/Application Support/Code/User` on macOS, and `%APPDATA%\Code\User` on
Windows. `<Claude config dir>` is `~/.config/Claude`,
`~/Library/Application Support/Claude`, or `%APPDATA%\Claude`.

The same catalog, including the exact entry setup writes for each client, is
available as JSON from any installed version:

```bash
contextstream-mcp clients --format json
npx -y @contextstream/mcp-server@latest clients --format json
```

## Add ContextStream with your client's own command

These commands connect the hosted server at `https://mcp.contextstream.io/mcp`
using the API key in `CONTEXTSTREAM_API_KEY`. Create a key at
[contextstream.io/account/api-keys](https://contextstream.io/account/api-keys).
The shell expands the variable, so the key is stored in the client's config.

A hosted connection does not write rules or hooks and does not sync your local
checkout. Use `setup` when you want those.

| Client | Command |
|---|---|
| Claude Code | `claude mcp add --transport http --scope user contextstream https://mcp.contextstream.io/mcp --header "X-ContextStream-API-Key: $CONTEXTSTREAM_API_KEY"` |
| OpenAI Codex CLI | `codex mcp add contextstream --url https://mcp.contextstream.io/mcp --bearer-token-env-var CONTEXTSTREAM_API_KEY` |
| Gemini CLI | `gemini mcp add --scope user --transport http --header "X-ContextStream-API-Key: $CONTEXTSTREAM_API_KEY" contextstream https://mcp.contextstream.io/mcp` |
| Qwen Code | `qwen mcp add --scope user --transport http --header "X-ContextStream-API-Key: $CONTEXTSTREAM_API_KEY" contextstream https://mcp.contextstream.io/mcp` |
| GitHub Copilot CLI | `copilot mcp add --transport http --header "X-ContextStream-API-Key: $CONTEXTSTREAM_API_KEY" contextstream https://mcp.contextstream.io/mcp` |
| Factory Droid | `droid mcp add contextstream https://mcp.contextstream.io/mcp --type http --header "X-ContextStream-API-Key: $CONTEXTSTREAM_API_KEY"` |

Codex reads the key from the environment each time it starts, instead of
storing it.

## Remote checks and verification hooks

Use a local MCP connection to run `check` or `verify` against your checkout.
You can also run a supported command with `contextstream-mcp run -- <command>`
from the checkout, including when your editor uses the hosted MCP connection.
Claude Code's managed Bash hook can route matching shell commands this way.

The managed session-start hooks in Claude Code and Cursor can send
`:prepare` in the background. With speculative checks on, a managed
post-tool hook can also schedule checks after it sees a successful supported
edit. A save that the hook doesn't observe won't schedule a check. Codex's
managed editor hooks handle notices and optional live progress. Use the local
MCP tool or the shell command for explicit checks in Codex.

When a verification gate is enabled, the managed pre-tool hooks can check
`git push`, `gh pr create`, `progress` reports marked `completed`, and
`entity` ticket updates to `resolved` or `closed`. These checks apply to the
calls the hook sees. Git's separate `pre-push` hook can block a push from any
editor or a terminal. It checks the committed trees Git is sending. Setup
installs it where Git capture is enabled. To refresh the managed Git hooks
in an existing checkout, run `contextstream-mcp git-hooks` there. It
preserves any existing user hooks. When a receipt is missing, the gate
reports the check to run before you retry.

Set the policy through `GET` and `PUT /api/v1/mcp/hosted-tools/gate-policy`.
Include `workspace_id`, `project_id` or both. Use query parameters for reads
and a JSON body for writes. A project policy can override its workspace's. Set
`require_verified_tree_before_push`, `require_verified_tree_before_pr` or
`require_verified_tree_before_complete` to turn on a gate. The policy can
set `required_command_families` and change the receipt age limit with
`receipt_ttl_secs`, which defaults to 24 hours.

[What remote checks and verification send](data-handling.md#remote-checks-and-verification).

## One-click install

VS Code asks for your API key when it first starts the server and keeps it in
its secret storage:

- [Install in VS Code](https://insiders.vscode.dev/redirect/mcp/install?name=contextstream&inputs=%5B%7B%22id%22%3A%22contextstream_api_key%22%2C%22type%22%3A%22promptString%22%2C%22description%22%3A%22ContextStream%20API%20key%22%2C%22password%22%3Atrue%7D%5D&config=%7B%22type%22%3A%22http%22%2C%22url%22%3A%22https%3A%2F%2Fmcp.contextstream.io%2Fmcp%22%2C%22headers%22%3A%7B%22X-ContextStream-API-Key%22%3A%22%24%7Binput%3Acontextstream_api_key%7D%22%7D%7D)
- [Install in VS Code Insiders](https://insiders.vscode.dev/redirect/mcp/install?name=contextstream&inputs=%5B%7B%22id%22%3A%22contextstream_api_key%22%2C%22type%22%3A%22promptString%22%2C%22description%22%3A%22ContextStream%20API%20key%22%2C%22password%22%3Atrue%7D%5D&config=%7B%22type%22%3A%22http%22%2C%22url%22%3A%22https%3A%2F%2Fmcp.contextstream.io%2Fmcp%22%2C%22headers%22%3A%7B%22X-ContextStream-API-Key%22%3A%22%24%7Binput%3Acontextstream_api_key%7D%22%7D%7D&quality=insiders)

## Kimi, GLM, and other models

ContextStream runs inside the client, so it works with whichever model the
client uses. Set it up for the client you run:

| You run | Setup |
|---|---|
| Kimi Code CLI | `contextstream-mcp setup --editors=kimi` |
| ZCode, Z.ai's GLM agent | `contextstream-mcp setup --editors=zcode` |
| Qwen Code | `contextstream-mcp setup --editors=qwen` |
| Muse Code on Muse Spark | `contextstream-mcp setup --editors=muse` |
| Claude Code on Kimi or GLM | `contextstream-mcp setup --editors=claude` |
| OpenCode or Crush on Kimi, GLM, Qwen, DeepSeek, or MiniMax | `contextstream-mcp setup --editors=opencode` or `--editors=crush` |

To run Claude Code on Kimi or GLM, point it at the provider's
Anthropic-compatible endpoint and put the provider's key in
`ANTHROPIC_AUTH_TOKEN`:

| Model | `ANTHROPIC_BASE_URL` |
|---|---|
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` |
| GLM (Z.ai) | `https://api.z.ai/api/anthropic` |

ContextStream's tools, rules, and hooks work the same way on either endpoint.
