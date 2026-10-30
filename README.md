<!-- mcp-name: io.github.contextstream/mcp-server -->

<p align="center">
  <img src="assets/contextstream-logo.png" alt="ContextStream" width="64" />
</p>

<h1 align="center">ContextStream MCP Server</h1>

<p align="center"><strong>The context platform for AI agents.</strong></p>

## Your agent knows code. Give it the decisions behind yours.

Connect your code, docs, and conversations so your agents can find the right
files, recall saved decisions, and build on past work across sessions and tools.

<a href="https://contextstream.io/benchmarks">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/contextstream/mcp-server/main/assets/benchmarks-dark.png">
    <img src="https://raw.githubusercontent.com/contextstream/mcp-server/main/assets/benchmarks-light.png" width="100%" alt="ContextStream benchmarks: LongMemEval-S 96.8% (484 of 500), LoCoMo 95.7%, code search 98.77% best answer in the top 10, agent tasks 96% (23 of 24, up from 58% without it)">
  </picture>
</a>

The numbers, their sources, and what they do not claim are under [Benchmarks](#benchmarks).

<a id="install-in-30-seconds"></a>

## Start free with MCP

**10,000 monthly credits. No credit card required.**

Works with Claude Code, Cursor, Codex, GitHub Copilot, Gemini CLI, Qwen Code,
Kimi Code, Muse Code, ZCode, Zed, and [more MCP clients](docs/clients.md), on
Claude, GPT, Gemini, Kimi, GLM, Qwen, and other models.
Create your account or sign in during MCP onboarding. No separate website signup needed.

**macOS and Linux**

```sh
curl -fsSL https://contextstream.io/scripts/mcp.sh | bash
```

**Windows PowerShell**

```powershell
irm https://contextstream.io/scripts/mcp.ps1 | iex
```

Paste the command into your terminal and follow onboarding to connect your
project and supported editor. Restart your editor after setup.

[What gets installed and indexed?](docs/data-handling.md) ·
[Supported clients](docs/clients.md) ·
[Setup guide](https://contextstream.io/docs/editors/setup) ·
[Prefer to start on the web?](https://contextstream.io/lp/mcp-marketplace?utm_source=github&utm_medium=directory&utm_campaign=mcp_organic_2026q3&utm_content=card_cta)

## Make the next session useful

Ask your connected agent:

> Use ContextStream to find the files relevant to my next change. Cite the
> sources and retrieve any saved project decisions that should guide the work.

Then ask it to save a real decision and its reason. Start a new session and
retrieve that decision without explaining it again. Indexing supplies code
context; it cannot recover every undocumented decision.

## Benchmarks

Each benchmark runs through the production API, is registered before its results
are read, and is published with its method, confidence interval, and losses.

| Benchmark | ContextStream | Compared with |
|---|---|---|
| **LongMemEval-S**, all 500 questions | **96.8%** (484/500); 96.0% on the half held out from tuning | Above Mastra (94.87%) and supermemory (85.4%) on the same official GPT-4o judge. Level with agentmemory's self-reported 96.2%, which is inside our 95% interval |
| **LoCoMo**, all 1,540 questions | **95.7%** (1474/1540); 95.8% held out; 97.8% on the audited answer key | Above every system in the EverMemOS paper's same-judge table: EverMemOS 93.05%, Zep 85.22%, MemOS 80.76%, MemU 66.67%, Mem0 64.20% |
| **Code search**, 177 judged developer queries | **98.77%** best answer in the top 10; **100%** a useful file in the top 10 | Embedding-only search 72.84% and ripgrep 42.59% on the same queries and judgments |
| **Agent tasks**, 8 tasks × 3 trials | **23/24** passed (96%); 9/9 on tasks that need team knowledge | 14/24 (58%) and 0/9 with the repository alone |

**61 ms median server-side search time** (127 ms p95) in the October 27 production
run. End to end it was 223 ms, because the client ran in a different region from the
one serving the workspace. Your latency depends on your distance from that region.

Competitor figures are as each vendor or the EverMemOS paper published them; we did
not re-run them, and readers and judges differ across rows. The code-search baselines
come from the August 1 run on the same queries. The agent-task result uses one
repository and one agent model, so treat it as directional.
[Results, methodology, and limitations](https://contextstream.io/benchmarks).

## ContextStudio

For people ContextStream Create offers one-prompt making to, the server adds one tool,
`studio`: ask for a film, a product explainer filmed from your connected product, an
image, sound or music in plain words, and ContextStudio's producer plans the pieces from
what your team knows (brand kit, docs, decisions, connected products). There are no models
to pick.

- **Brand work by name.** Brand images (social sets in each network's size, ad sets, hero
  and product shots, posters and flyers with print PDFs, email headers, brand graphics) and
  brand sounds (sonic logos, music beds, loops, effect packs, voice-overs, podcast intros
  and outros): pass `form`, with `sizes` (up to 6, such as `instagram-post` or
  `linkedin-post`) or a `voice`, and the plan keeps to it.
- **Plan, then price.** `plan` returns the pieces and one price ("up to N credits, likely
  about M") and spends nothing. Pass what your agent knows that the workspace may not (the
  diff it just made, release notes, the doc it has open) as `context`; the producer reads
  it next to the team's own context.
- **Your go-ahead.** `make` spends at most the plan's price, and only after you agree: a
  client that can ask you (MCP elicitation, or a stateless `input_required` result) asks
  you itself; otherwise the agent passes the price you accepted.
- **Following it.** `status` reports each piece while it waits (with
  `notifications/progress` where the call asks for them); a product film takes 13 to 21
  minutes. `result` shows images inline and links video and sound (signed links valid
  about 15 minutes), with each piece's placard. Brand work comes as files, each with its
  own link: every size as PNG, JPG and WebP with a ZIP of the set, a print PDF, or WAV and
  MP3 for the web with a broadcast master and stems, plus alt text, a caption and the
  checks it was held to.
- **Revising.** `revise` starts from a made image or sound: a `variation` (more like it), a
  `revision` (it, changed as you say) or a `resize` (an image in new `sizes`). Also `stop`,
  `library`, `brand` and `products`.
- **Email, where Create offers it.** Campaigns sent through your team's own Resend account.
  `email_draft` writes one in your brand from a prompt ("announce our winter sale to
  customers") and sends nothing; `email_audiences`, `email_campaigns`, `email_test` (to your
  own address or Resend's test addresses), `email_cancel` and `email_report` (a campaign's
  delivery, opens, clicks, bounces and complaints, or the workspace's trends). `email_send`
  goes only with your go: the client asks you when it can; otherwise the agent shows you the
  campaign and its recipient count and passes the count you agreed to.
  `email_preview` gives a link to share that shows the email in Gmail, Outlook and Apple Mail
  mockups (desktop, tablet and phone, light and dark, images on or off), with the inbox line
  and what to check. `email_revise` changes a draft from plain words ("shorter subject") or
  sets its subject and preview line; `email_duplicate` makes a draft copy. `email_contacts`
  lists, adds (with where they agreed to hear from you), erases or unsubscribes contacts;
  `email_unsubscribes` shows the suppression list with why and when, and takes an address off
  it only with your go. Resend is connected in the studio, never through an agent: the tool
  refuses anything shaped like a Resend key.
- **Connector.** The hosted path `/studio` serves only this tool, for Claude.ai or ChatGPT
  connectors, with OAuth tokens bound to that resource (`read:studio`, `write:studio`).

## Data controls

**Scoped access. Traceable sources. Configurable capture.**
Indexing sends eligible source contents to ContextStream for hosted search.
Transcript saving and local Git metadata capture are on by default and can be
turned off. Review [data handling and controls](docs/data-handling.md) before
connecting sensitive projects.

<details>
<summary><strong>Other installation options and troubleshooting</strong></summary>

### npm (Node.js 20+)

```bash
npx -y @contextstream/mcp-server@latest setup
```

The npm launcher downloads the native Rust binary for your platform and verifies
its SHA-256 checksum. Pin an exact package version for production automation.

### Hosted MCP without a local process

For clients supporting Streamable HTTP and OAuth, the hosted endpoint is:

```text
https://mcp.contextstream.io/mcp?default_context_mode=fast
```

[Supported clients](docs/clients.md) lists one-line add commands for Claude
Code, Codex, Gemini CLI, Qwen Code, Copilot CLI, and Droid, and one-click
install for VS Code. Use the [MCP documentation](https://contextstream.io/docs/mcp)
for other clients and stdio alternatives. A hosted connection alone does not
sync your local checkout.

### Preview or diagnose setup

```bash
contextstream-mcp setup --dry-run
contextstream-mcp doctor
contextstream-mcp doctor --repair
```

### Scripted and CI setup

For automation, wrap the install in `pipefail` so it fails when the download
fails, instead of reporting success after installing nothing:

```bash
bash -o pipefail -c 'curl -fsSL https://contextstream.io/scripts/mcp.sh | bash'
```

`setup --yes` reads the key from `CONTEXTSTREAM_API_KEY` or saved credentials,
and needs `--workspace-id` when the account has more than one workspace:

```bash
export CONTEXTSTREAM_API_KEY=...   # or: printf %s "$KEY" | contextstream-mcp configure --api-key-stdin
contextstream-mcp setup --yes --editors=claude --workspace-id=<UUID> --project-path=.
```

### Package Bouncer (opt-in)

A hook for Claude Code and Cursor that checks the packages an install command names
against their public registry before the command runs. It covers npm, pnpm, yarn, bun,
npx, pip, uv, uvx, pipx, poetry and cargo, including `claude mcp add ... -- npx ...`.

- It **denies** a package the registry does not have, naming the popular package it
  looks like. An agent may have invented the name, and anyone can register it.
- It **asks** before a barely downloaded lookalike of a popular package.
- It **allows** everything else, including whenever a lookup fails or takes too long.

It skips projects and commands that use a private registry (`.npmrc`, `pip.conf`, uv or
poetry sources, `--registry`, `--index-url`). `CONTEXTSTREAM_PACKAGE_BOUNCER=off`
turns it off.

Claude Code: add this entry to `PreToolUse` in `~/.claude/settings.json`, next to the
hooks setup installed. `update-hooks` leaves it alone.

```json
{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "contextstream-mcp hook package-bouncer", "timeout": 8 }] }
```

Cursor (`~/.cursor/hooks.json`):

```json
{ "version": 1, "hooks": { "beforeShellExecution": [{ "command": "contextstream-mcp hook package-bouncer" }] } }
```

</details>

## Open source and contributing

This repository contains the MIT-licensed Rust MCP server, client, editor setup,
and release tooling. The hosted backend is separate and is not included here.
See the [architecture](docs/architecture.md), [release integrity](docs/release.md),
and [latest release](https://github.com/contextstream/mcp-server/releases/latest).

```bash
cargo build --locked -p mcp-server --bin contextstream-mcp
cargo test --locked --workspace
```

Use the pinned Rust toolchain. For contribution checks and commit sign-off, read
[CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities using
[SECURITY.md](SECURITY.md). See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[GOVERNANCE.md](GOVERNANCE.md) for licensing, trademarks, and project governance.

---

**Let humans be human.**

[Docs](https://contextstream.io/docs/mcp) ·
[Pricing](https://contextstream.io/pricing) ·
[Integrations](https://contextstream.io/docs/integrations) ·
[Benchmarks](https://contextstream.io/benchmarks)

### Optional deep project learning

Deep project learning starts off for new accounts. Interactive `setup` offers it
once per account (or once when terms change) and remembers if you decline; plain
`contextstream-mcp setup --yes` preserves your current choice.

```bash
contextstream-mcp configure --account-learning status   # show current consent state
contextstream-mcp configure --account-learning on       # open privacy page in browser (or print URL)
contextstream-mcp configure --account-learning off      # withdraw consent and queue deletion
```

`--deep-learning` is accepted as an alias for `--account-learning`. Only a person
can grant consent by ticking the unchecked box on the signed-in privacy page
(`https://contextstream.io/dashboard/account/privacy`); agents and API keys cannot
consent for you. Doctor and init show status only. See
[data handling and controls](docs/data-handling.md).
