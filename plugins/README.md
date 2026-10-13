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

Anyone on the Coach pilot can add it to ChatGPT as a custom connector. Turn on developer mode in ChatGPT's connector settings, create a connector with the URL `https://mcp.contextstream.io/coach` and OAuth, and sign in to ContextStream when asked. Developer mode is only needed for this private route; once the plugin is published, people install it from the directory without it.

## Submit it (owner steps)

These steps follow OpenAI's submission guide (developers.openai.com/plugins/deploy/submission). Nothing here has been submitted.

1. **Who submits.** An organization owner, or a member with Apps Management Write. Complete individual or business verification first and choose the verified developer identity (ContextStream) for the draft.
2. **Verify the domain.** OpenAI gives a challenge token. Serve exactly that token as plain text at `https://mcp.contextstream.io/.well-known/openai-apps-challenge`, or at the parent `https://contextstream.io/.well-known/openai-apps-challenge`. The hosted gateway and the edge worker don't serve this path yet.
3. **Make the reviewer account.** A dedicated test account with a password sign-in, never a person's real account. It must work at once, with no MFA, email or SMS codes, magic links or private network. While Coach is a pilot, its email must be on `COACH_PILOT_EMAILS` (or open Coach to everyone before the review). Keep it available for later reviews.
4. **Reviewer access goes in the dashboard,** in the Review details form. Never put credentials in the ZIP; the manifest rejects them.
5. **Record the walkthrough.** A video of the eight review cases, run with the reviewer account, at an accessible URL. Add it as `review.demo_recording_url` in `plugin.json` (and rebuild), or in the dashboard.
6. **Choose the countries** (`publication.countries`, uppercase codes such as `US` and `GB`) if it shouldn't be available everywhere.
7. **Upload the ZIP** and choose "With MCP":
   - URL `https://mcp.contextstream.io/coach`, authentication OAuth. Dynamic client registration works today; switch `mcp.json` to `"mode": "cimd"` once ContextStream's authorization server advertises `client_id_metadata_document_supported`.
   - Scan the tools. Expect `workflow` (read-only), `workflow_save`, and `workflow_delete` (destructive, asks for confirmation).
   - Content security: the card loads nothing from the network, so no connect or resource domains are needed.
8. **Run all eight cases** with the reviewer account before submitting: five where the plugin should act, three where it should not (a one-off poem, deleting without confirmation, posting to Slack or email).
9. **Complete the policy attestations and submit.** Only one review can be open at a time; feedback arrives by email.
10. **Publish** when you choose, after approval.

After publishing, OpenAI scans the MCP server daily and takes eligible tool changes after automated checks; new tools wait for approval. Changing the listing, the icons or the skill needs a new ZIP and review. Changing the MCP server's URL needs OpenAI support.

## What the package can't check

The privacy policy at `https://contextstream.io/privacy` should say how coaching data is kept, exported and deleted (a deleted workflow is hidden at once and erased within 30 days), and that it is never used to train models.
