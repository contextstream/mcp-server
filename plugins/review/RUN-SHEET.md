# Agent Coach Kit: running the eight review cases

OpenAI's review asks for a video of the plugin's test cases run with the reviewer account (README step 5).
This sheet is the script for that run. The five positive and three negative cases are the ones in
`plugin.json` (`extensions.com.openai.review.test_cases`); the prompts below are copied from it and a test
keeps them equal.

## Before you start

- **Reviewer account.** A dedicated test account with a password sign-in and no MFA, email or SMS codes. It
  needs its own ChatGPT account too. Its ContextStream email must be on `COACH_PILOT_EMAILS`, or Coach must be
  open to everyone (`*`). Without that, the plugin signs in but every call answers `coach_not_enabled`.
- **Empty account.** No workflows yet, so case 1 starts from nothing. `dry_run.py` leaves nothing behind.
- **Install the way a reviewer does:** upload the ZIP (`python3 plugins/build_agent_coach_kit.py`), choose
  "With MCP", and scan: expect `workflow` (read-only), `workflow_save` and `workflow_delete` (destructive).
- **Check the tools first, without ChatGPT:**

  ```sh
  CONTEXTSTREAM_API_KEY=<the account's key> python3 plugins/review/dry_run.py
  ```

  It runs the five positive flows and the delete refusal against the live endpoint with a throw-away
  workflow and deletes it. All lines should say `PASS`. Last run: 2026-10-14 against MCP 1.0.24, all pass.

## What ChatGPT will do that is not the plugin

- ChatGPT asks once per conversation before `workflow_save` ("Allow once" / "Always allow"). Allow it; that
  is its own permission card, and reads (`workflow`) need none.
- Do not word a request as a "recurring" task. ChatGPT may then also create its own scheduled task in the
  account's Scheduled list. That is ChatGPT's feature, not the plugin's, but it looks like an action nobody
  asked for. The prompts below avoid it.

## The cases

Run 1 to 3 in one chat. Run 4 in a **new** chat (the point is a session that remembers nothing). Run 5 in
the same chat as 4. Run the negatives in a new chat each.

| # | Prompt (type it exactly) | What the video must show |
| --- | --- | --- |
| 1 | Every Monday I write a short update for my leadership team: what changed, who is affected and what they need to do. Set that up as a workflow I can coach. | `workflow` lists, one line of goal is proposed and only missing details are asked, then `workflow_save` creates it. "Workflow created" and the card. |
| 2 | Start this week's update: the billing migration finished and invoices now use the new layout. | The job's objective in one line, `start_job`, the context is read, a draft is written and **saved as a version before it is shown**, with its reference (for example `leadership-update#1/v1`) and the suggestion buttons. |
| 3 | Too long. Every time, open with what leadership needs to do. Just for this week, mention the Friday maintenance window. | Two reactions saved without being asked to: the first as reusable (it becomes guidance), the second for this job only. One line says which is which. Then a revised version that opens with the action and mentions the window. |
| 4 | Write next week's leadership update: the new weekly report ships on Monday. (new chat) | The workflow and the new job's context are read; the draft opens with what leadership needs to do; it does **not** mention the Friday window; it is saved as a version. |
| 5 | Compare it with last week's. This one is better: I didn't have to repeat myself. | Both versions side by side with no score; then the verdict "better" is recorded with the person's words. It never picks a verdict itself. |
| N1 | Write me a short poem about autumn. | No tool is called; nothing is created or saved. |
| N2 | Delete my leadership update workflow. | It asks for the person's explicit confirmation of that exact workflow and deletes nothing until given it. Then **answer no**, and show the workflow is still listed. |
| N3 | Post this week's update to our company Slack and email it to the whole leadership team. | It says the plugin cannot send or publish anything and does not claim to have sent it. No tool call posts or emails (none exists). |

## Which parts the dry run proves

| Case | Dry run (tool level) | Only the ChatGPT run can show |
| --- | --- | --- |
| 1 | the workflow is created and saved | that ChatGPT chooses the plugin and proposes the goal in one line |
| 2 | a job starts, a draft saves as a version, the context carries an etag | the order: saved before shown |
| 3 | reusable and this-job-only feedback are kept apart; a revision saves | that ChatGPT saves them unprompted and says which is which |
| 4 | the next job's context carries the guidance and not the one-off | that the draft opens with the action and omits the window |
| 5 | compare shows both versions with no score; a verdict records in the person's words | that ChatGPT leaves the verdict to the person |
| N1 | nothing (no tool is involved) | no tool call |
| N2 | an unconfirmed or wrongly confirmed delete is refused and the workflow stays | that ChatGPT asks first |
| N3 | only three tools exist; none can post or email | that ChatGPT declines and claims nothing |

## After the run

- Add the video's URL as `review.demo_recording_url` in `plugin.json`, rebuild, upload.
- Delete the reviewer account's workflows from the dashboard, or leave them: the reviewer may look at them.
- Keep the reviewer account available while the review is open; feedback arrives by email.
