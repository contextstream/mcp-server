# Agent Coach Kit plugin

The package for the plugin directory that ChatGPT and Codex share. It connects to the hosted Agent Coach Kit endpoint, `https://mcp.contextstream.io/coach`, and signs people in to their own ContextStream account with OAuth.

| Path | What it is |
| --- | --- |
| `agent-coach-kit/plugin.json` | The Agent Plugins manifest: listing text, links, icons, the five positive and three negative review cases, release notes |
| `agent-coach-kit/mcp.json` | The one remote MCP server, with OAuth (dynamic client registration) and the `read:coach` and `write:coach` scopes |
| `agent-coach-kit/skills/agent-coach/SKILL.md` | The coaching skill, written for the `workflow`, `workflow_save` and `workflow_delete` tools. Its rules match the Codex Cloud kit's |
| `agent-coach-kit/assets/` | Square SVG icon and logo |
| `build_agent_coach_kit.py` | Checks the package against the submission rules it can check, then writes `dist/agent-coach-kit-<version>.zip` |
| `tests/` | `python3 -I -m unittest discover -s plugins/tests -v` |

## Build the ZIP

```sh
python3 plugins/build_agent_coach_kit.py            # plugin.json at the root of the archive
python3 plugins/build_agent_coach_kit.py --folder   # inside one agent-coach-kit/ folder
```

OpenAI's guide says only that `plugin.json` sits at the plugin root. Upload the first ZIP; if the portal can't find the manifest, upload the `--folder` one.

## Use it before it is published

Anyone on the Coach pilot can add it to ChatGPT by its address. In ChatGPT open **Plugins**, choose **Add**, then **Add custom MCP server**. Give it a name, the server URL `https://mcp.contextstream.io/coach`, leave authentication on OAuth, confirm the risk notice and choose **Create as a plugin**. ChatGPT shows its connection notice; continue to ContextStream's consent page, which asks only to read and save your coaching, and authorize. On the ChatGPT Plus account this was verified on (2026-10-13) no developer-mode switch was needed, and ChatGPT labels a plugin added this way "dev mode".

Codex signed in with the same ChatGPT account sees the plugin with no further step. Codex can also connect on its own: `codex mcp add agent-coach-kit --url https://mcp.contextstream.io/coach` finds the OAuth sign-in and opens the same consent page; its "Open local callback" button hands the sign-in back to the terminal.

Once the plugin is published, people install it from the directory without typing an address.

## Submit it (owner steps)

These steps follow OpenAI's submission guide (developers.openai.com/plugins/deploy/submission). Nothing here has been submitted.

1. **Who submits.** An organization owner, or a member with Apps Management Write. Complete individual or business verification first and choose the verified developer identity (ContextStream) for the draft.
2. **Verify the domain.** OpenAI gives a challenge token. Serve exactly that token as plain text at `https://mcp.contextstream.io/.well-known/openai-apps-challenge`, or at the parent `https://contextstream.io/.well-known/openai-apps-challenge`. The edge worker serves the value of its `OPENAI_APPS_CHALLENGE` secret on the parent domain and the API origin (`mcp.contextstream.io` itself answers 404 for it, and the parent domain is allowed). Set that secret to OpenAI's token with wrangler, then fetch the URL and check it returns exactly the token.
3. **Make the reviewer account.** A dedicated test account with a password sign-in, never a person's real account. It must work at once, with no MFA, email or SMS codes, magic links or private network. While Coach is a pilot, its email must be on `COACH_PILOT_EMAILS` (or open Coach to everyone before the review). Keep it available for later reviews.
4. **Reviewer access goes in the dashboard,** in the Review details form. Never put credentials in the ZIP; the manifest rejects them.
5. **Record the walkthrough.** A video of the eight review cases, run with the reviewer account, at an accessible URL. Add it as `review.demo_recording_url` in `plugin.json` (and rebuild), or in the dashboard.
6. **Choose the countries** (`publication.countries`, uppercase codes such as `US` and `GB`) if it shouldn't be available everywhere.
7. **Upload the ZIP** and choose "With MCP":
   - URL `https://mcp.contextstream.io/coach`, authentication OAuth. `mcp.json` uses ChatGPT's Client ID Metadata Document (`"mode": "cimd"`), which ContextStream's authorization server supports since 2026-10-13 (it advertises `client_id_metadata_document_supported`). Dynamic client registration still works as a fallback.
   - Scan the tools. Expect `workflow` (read-only), `workflow_save`, and `workflow_delete` (destructive, asks for confirmation).
   - Content security: the card loads nothing from the network, so no connect or resource domains are needed.
8. **Run all eight cases** with the reviewer account before submitting: five where the plugin should act, three where it should not (a one-off poem, deleting without confirmation, posting to Slack or email). [`review/RUN-SHEET.md`](review/RUN-SHEET.md) has the exact prompts and what each must show; `review/dry_run.py` checks the tool behaviour first, without ChatGPT.
9. **Complete the policy attestations and submit.** Only one review can be open at a time; feedback arrives by email.
10. **Publish** when you choose, after approval.

After publishing, OpenAI scans the MCP server daily and takes eligible tool changes after automated checks; new tools wait for approval. Changing the listing, the icons or the skill needs a new ZIP and review. Changing the MCP server's URL needs OpenAI support.

## What the package can't check

The privacy policy at `https://contextstream.io/privacy` should say how coaching data is kept, exported and deleted (a deleted workflow is hidden at once and erased within 30 days), and that it is never used to train models.
