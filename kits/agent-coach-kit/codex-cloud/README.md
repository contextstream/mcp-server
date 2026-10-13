# Agent Coach Kit for Codex Cloud

Coach your Codex agent on work you need again and again: a weekly note, an article, a recap, a series of images.

- A **workflow** has a goal: what good looks like every time.
- Each **job** has an objective: what this one must achieve.
- Every draft is saved as a **version**: text, a link, or an image, audio, video or document.
- Your **feedback** is kept in your own words on the exact version you meant. It is either reusable (later jobs too) or for this job only.
- Reusable feedback becomes **guidance** that comes back in the next job, even in a fresh task. One-off feedback stays with its job.

Everything is saved in your own ContextStream account.

> **Using ChatGPT?** ChatGPT plugins also work in Codex cloud tasks. When the Agent Coach Kit plugin is available, use it instead: it signs in to your ContextStream account itself, so it needs no API key at all.

## What's here

| File | Goes in your repo at |
| --- | --- |
| `.agents/skills/contextstream-coach/SKILL.md` | the same path |
| `.agents/skills/contextstream-coach/scripts/coach.py` | the same path |
| `AGENTS.snippet.md` | paste into your repo's `AGENTS.md` |

`coach.py` uses only the Python standard library, so it runs on Codex Cloud's Python without an install step.

## Set up Codex Cloud

1. **Get an API key.** Sign in at contextstream.io (or create an account), then go to Account → API keys and create a key just for this. You can revoke it at any time.
2. **Create an environment.** In the Codex web app, open **Work**, then **Work in Cloud**, then **Choose environment**, then **Create environment** for your repo.
3. **Add the key as a network secret.** Under network secrets, add `CONTEXTSTREAM_API_KEY` with your key as the value and `api.contextstream.io` as its allowed domain. Tasks only ever see a placeholder; the network proxy puts the real key on requests to `api.contextstream.io` and nowhere else. `coach.py` sends it as the `X-API-Key` header and never prints it.
4. **Publish the environment.**
5. **Commit the skill.** Copy `.agents/skills/contextstream-coach/` into your repo and paste `AGENTS.snippet.md` into your `AGENTS.md`. Codex Cloud reads skills from the repo, not from your personal skills folder.
6. **Check it.** Start a task and ask Codex to run `python3 .agents/skills/contextstream-coach/scripts/coach.py doctor` with network access. It checks the key, the network and that Coach is turned on for your account, without showing the key.

### Network access

Every `coach.py` command needs network access, and Codex Cloud runs commands without it unless it's enabled. The skill tells the agent to run its commands with network access. A command that ran without it prints

```
[COACH_NOT_SAVED] this command needs network access; run it again with network enabled
```

and exits with code 6. Nothing was saved; running the same command again with network enabled is safe.

If your environment limits internet access to a list of domains, include `api.contextstream.io` and allow all HTTP methods: `coach.py` saves with POST, PUT, PATCH and DELETE.

### Images, audio, video and documents

- **Up to 8 MiB:** the file goes to `api.contextstream.io` with the request, so nothing else needs allowing.
- **Over 8 MiB:** the file is uploaded straight to ContextStream's file storage through a short-lived upload link, on a host under `r2.cloudflarestorage.com`. `coach.py` prints the exact host before it uploads (`Uploading 9.3 MiB directly to <host>`), and names it again if the upload is blocked. To send big files, add that host to the environment's allowed domains; otherwise keep files under 8 MiB. The API key is never sent to the storage host.

## How a job goes

Ask Codex something like: "Start a new job for my weekly-note workflow: this week's update on the onboarding release."

- It proposes the job's objective in one line, from your goal and recent jobs, and asks only about what changed. For a new workflow it does the same with the goal.
- It writes the draft with your guidance and approved examples, saves it as a version, and shows it to you.
- You react in your own words. It saves your words on that version without asking, says whether it saved them for later jobs too or just this one, and you can change that.
- Each revision says which guidance and feedback it applied, kept or adapted; what it applied or kept stops showing as still needing attention.
- It revises until you're happy, then closes the job with the version you used. If you compare with the last job, it records your verdict (better, same or worse) in your words as soon as you give it; it never picks one for you.

Next week, in a fresh task, the reusable feedback comes back on its own; one-off requests stay with the job they were about.

## Commands

Run them as `python3 .agents/skills/contextstream-coach/scripts/coach.py ...`. `coach.py --help` and `coach.py COMMAND --help` list every option.

| Command | What it does |
| --- | --- |
| `doctor` | checks the key, the network and that Coach is on |
| `list` | your workflows |
| `get REF` | a workflow (recent objectives, guidance, open items, coaching effort), job, version, feedback or guidance |
| `create --name --goal` | a new workflow |
| `start-job WORKFLOW --objective` | starts a job and prints its context |
| `context JOB` | the context for an open job |
| `save-output JOB --file PATH` | saves a text draft as a new version (stdin without `--file`) |
| `save-output JOB --url https://... [--file PATH]` | saves a link, with an optional text snapshot |
| `save-output JOB --media PATH` | saves an image, audio, video or document |
| `save-output ... --carried g1:applied --carried f2:kept` | says which guidance (`g1`) or feedback (`f2` this job's, `#3/f1` an earlier job's) the version carried, and how: kept, adapted, applied or not_applied |
| `add-feedback VERSION --kind correction\|approval\|direction --scope reusable\|this-job --text "..." [--reading "..."]` | records your words verbatim, with the agent's reading kept apart; also `--excerpt`, `--from 0:34 --to 0:41`, `--repeat-of GUIDANCE`, `--picked` |
| `rescope FEEDBACK --scope this-job\|reusable` | changes whether feedback reaches later jobs |
| `retract-feedback FEEDBACK` | takes feedback back |
| `set-effort WORKFLOW\|JOB --effort quick\|explore\|clear --said "..."` | the coaching effort you asked for, in your words |
| `close-job JOB --used VERSION [--verdict better\|same\|worse --note "..."]` | closes a job with the version you used and your verdict |
| `compare JOB` | this job's latest version next to the last job's |
| `verdict JOB --verdict better\|same\|worse --said "..." [--candidate VERSION]` | records your verdict on the comparison, in your words, before or after the job is closed |
| `refine-goal WORKFLOW --goal --reason` | a new goal version |
| `edit-guidance GUIDANCE --text "..." [--applies-when "..."]` | edits guidance or limits where it applies |
| `retire-guidance GUIDANCE [--remove\|--restore]` | retires, removes or restores guidance |
| `import-guidance GUIDANCE --into WORKFLOW` | reuses guidance from another of your workflows |
| `history WORKFLOW` | goal versions, guidance and all feedback |
| `metrics WORKFLOW` | how the coaching is going, from real jobs: versions and feedback per job, repeated corrections, your verdicts, and the context audit; no quality score |
| `export WORKFLOW --format markdown\|json [--out PATH]` | everything in a workflow |
| `delete-workflow WORKFLOW --confirm SLUG` | deletes a workflow; refused unless the confirm is its slug |

Every change keeps its history. Deleting a workflow hides it at once and erases it, with everything in it, within 30 days; deleting your ContextStream account erases everything. `export` first if you want a copy.

### Saves are never doubled

Every save carries an operation id made from the command and its arguments. Running the same command again, after a network error or in a later step, reuses the id, and ContextStream answers with the first save instead of saving twice. Each save prints one of:

| Tag | Meaning |
| --- | --- |
| `[COACH_SAVED]` | saved |
| `[COACH_ALREADY_SAVED]` | this exact save was already made; nothing new was saved |
| `[COACH_CONFLICT]` | someone changed it first, or the id was used for a different request; nothing changed |
| `[COACH_NOT_SAVED]` | not saved; the message says why, and running the same command again is safe |

`start-job` differs on purpose: the same objective starts a new job after 10 minutes, so a recurring job can reuse its wording.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | done |
| 1 | anything else; the message says what |
| 2 | wrong arguments, or a delete without the right confirmation |
| 3 | the key was refused, or Coach isn't turned on for the account |
| 4 | conflict: nothing changed |
| 5 | the network blocked the host: check the environment's allowed domains |
| 6 | no network access: run the command again with network enabled |

## Tests

```sh
python3 -I -m unittest discover -s kits/agent-coach-kit/codex-cloud/tests -v
```

They run against a local stub of the API and make no network calls.
