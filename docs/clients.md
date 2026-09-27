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
