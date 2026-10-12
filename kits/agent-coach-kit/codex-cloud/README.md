# ContextStream Coach for Codex Cloud

Coach your Codex agent on recurring work: a weekly note, an article, a recap, a series of images. You give feedback on its drafts; it keeps your exact words, scoped to the whole workflow or to one job, and brings the right feedback back the next time. Everything is saved in your own ContextStream account.

## What's here

| File | Goes in your repo at |
| --- | --- |
| `.agents/skills/contextstream-coach/SKILL.md` | the same path |
| `.agents/skills/contextstream-coach/scripts/coach.py` | the same path |
| `AGENTS.snippet.md` | paste into your repo's `AGENTS.md` |

`coach.py` uses only the Python standard library, so it runs on Codex Cloud's Python without an install step.

## Set up the Codex Cloud environment

1. **Get an API key.** Sign in at contextstream.io (or create an account), then go to Account → API keys and create a key just for this. You can revoke it at any time.
2. **Add it as a network secret.** In the environment's settings, add a network secret named `CONTEXTSTREAM_API_KEY` with your key, and set its allowed domain to `api.contextstream.io`. Programs only ever see a placeholder; Codex swaps in the real key on requests to that domain.
3. **Internet access.** Choose "Custom domains only". The network secret already allows `api.contextstream.io`. Allow all HTTP methods: the helper saves with POST and PATCH.
4. **Large media (optional).** Images, audio, video and documents up to 8 MiB go to `api.contextstream.io` directly. Larger files are uploaded straight to ContextStream's storage host; add that host to the allowlist if you need it. `coach.py` names it in its error message if an upload is blocked.
5. **Commit the skill.** Copy `.agents/skills/contextstream-coach/` into your repo and paste `AGENTS.snippet.md` into your `AGENTS.md`. Codex Cloud reads skills from the repo, not from your personal skills folder.
6. **Check it.** Start a task and ask Codex to run `python3 .agents/skills/contextstream-coach/scripts/coach.py doctor`. It checks the key, the network and that Coach is turned on for your account, without ever printing the key.

## How a job goes

Ask Codex something like: "Start a new job for my weekly-note workflow: this week's update on the onboarding release."

- It starts the job and reads what it needs: your goal, the guidance you've given, and last week's draft.
- It writes the draft, saves it as a version, and shows it to you.
- You give feedback in your own words. It records them on that exact version and asks whether they apply to later jobs too or just this one.
- It revises until you're happy. When you close the job, it asks you how this draft compares with the last job's.

Next week, in a fresh task, the reusable feedback comes back on its own; one-off requests stay with the job they were about.

## Commands

`coach.py --help` lists them all. The main ones:

| Command | What it does |
| --- | --- |
| `list` | your workflows |
| `create --name --goal` | a new workflow |
| `start-job WORKFLOW --objective` | starts a job and prints its context |
| `context JOB` | the context for an open job |
| `save-output JOB --title --file\|--media [--revises] [--carry-note]` | saves a draft as a new version |
| `add-feedback VERSION --kind correction\|approval\|direction --scope reusable\|this-job --text` | records your feedback word for word |
| `compare JOB` | this job's latest draft next to the last job's |
| `close-job JOB --verdict better\|same\|worse` | closes a job with your verdict |
| `refine-goal`, `edit-guidance`, `retire-guidance`, `retract-feedback`, `history` | change or review what was saved |

Nothing is ever deleted: every change keeps its history.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | done |
| 2 | wrong arguments |
| 3 | the key was refused, or Coach isn't turned on for the account |
| 4 | conflict: someone changed it first; nothing changed |
| 5 | network blocked: check the environment's internet access |
| 1 | anything else; the message says what |

## Tests

```sh
python3 -m unittest discover -s kits/agent-coach-kit/codex-cloud/tests -v
```

They run against a local stub of the API and make no network calls.
