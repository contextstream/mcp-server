//! AI rules generation for editors.
//!
//! Generates editor-specific rules files for ContextStream integration.
//! Supports multiple editors with different rule formats:
//! - Claude Code: MCP tool names prefixed with `mcp__contextstream__`
//! - Other MCP editors: Raw tool names (init, context, search, etc.)
//! - No-hook editors (Codex, OpenCode, Aider, Antigravity): Extra guidance for manual enforcement

use super::safe_edit;
use anyhow::{Context, Result};
use mcp_types::{build_harness_teaching, HarnessTeachingDelivery, HARNESS_TEACHING_VERSION};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::editors::Editor;

/// ContextStream block markers (XML-style — LLMs pay more attention to these).
const CONTEXTSTREAM_START: &str = "<contextstream>";
const CONTEXTSTREAM_END: &str = "</contextstream>";
const RULES_HASH_MARKER_PREFIX: &str = "<!-- contextstream-rules-hash:";

/// Legacy markers for backward compatibility during replacement.
const LEGACY_START: &str = "<!-- BEGIN ContextStream -->";
const LEGACY_END: &str = "<!-- END ContextStream -->";

/// Canonical long-form project rules file for editors that support references.
const SHARED_PROJECT_RULES_PATH: &str = ".contextstream/rules.md";
const COPILOT_SKILL_PATH: &str = ".github/skills/contextstream-workflow/SKILL.md";
#[cfg(test)]
const COPILOT_SKILL_LEGACY_MARKER: &str = "<!-- ContextStream managed Copilot skill v1 -->";
const COPILOT_SKILL_HASH_MARKER_PREFIX: &str =
    "<!-- ContextStream managed Copilot skill v2 sha256: ";
const COPILOT_SKILL_HASH_MARKER_SUFFIX: &str = " -->";
const COPILOT_SKILL_HASH_PLACEHOLDER: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// SHA-256 fingerprints of every distinct canonical Copilot skill emitted by
/// released binaries before the self-verifying v2 marker. Exact fingerprints
/// keep migration fail-closed: a user edit changes the fingerprint and is
/// therefore never overwritten merely because a managed marker remains.
const LEGACY_COPILOT_SKILL_SHA256: &[&str] = &[
    // v0.1.89-v0.2.50 (unmarked)
    "9413ce50335fd6c5515dff2cd3ce542bb743cd76f6993c789d7c14054ce9a7a4",
    // v0.2.51 (unmarked)
    "9c5d9bb022df93aff1d62a000a043de1aac289d6a4084e4ad37fab28e6d84261",
    // v0.2.52-v0.3.55 (unmarked)
    "62962ef97f9a6a558fc2128e295ed680311e29f8ff50545637e344842db2c64b",
    // v0.3.56-v0.5.61 (unmarked)
    "5b26171fa294282b87f7aa8c7cb1404e77b319995de159493c8e2e36441b8f39",
    // v0.5.62-v0.5.82 (v1 marker)
    "d92b025b0bb8551fa3a1b14161f04263170088f71a64a4630ced8971c6e685a3",
    // v0.5.83-v0.5.85 (v1 marker + canonical handoff guidance)
    "f394bb3bd849884ee4269acfed10c20f4f7ba8b1e4018104ad4c0451b53928f7",
    // v0.5.86 (v1 marker + explicitly scoped handoff call)
    "4571df4c51a7347e9617dc12b87810d6f87ce84fbfef36a3fd01f6d517b0d758",
];
const DEFAULT_WORKSPACE_NAME: &str = "Workspace";
const DEFAULT_WORKSPACE_ID: &str = "00000000-0000-0000-0000-000000000000";
const RULES_BUNDLE_FINGERPRINT_VERSION: &str = "contextstream-rules-bundle-v1";

static CANONICAL_RULES_BUNDLE_HASH: OnceLock<String> = OnceLock::new();

/// Windsurf rule frontmatter for always-on activation.
const WINDSURF_ALWAYS_ON_FRONTMATTER: &str = "---\ntrigger: always_on\n---\n\n";

/// Cursor `.mdc` frontmatter for always-on activation. `alwaysApply: true`
/// makes Cursor inject the rule into every chat/composer/agent session — the
/// only reliable always-on mechanism now that Agent mode ignores `.cursorrules`.
const CURSOR_MDC_FRONTMATTER: &str = "---\ndescription: ContextStream - persistent memory, plans, tasks, lessons, docs, media, and code search. Always on.\nalwaysApply: true\n---\n\n";

/// MCP tool prefix for Claude Code.
const CLAUDE_MCP_PREFIX: &str = "mcp__contextstream__";

/// Tool names that need prefixing for Claude Code.
const TOOL_NAMES: &[&str] = &[
    "init",
    "context",
    "instruct",
    "flash",
    "ram",
    "search",
    "session",
    "memory",
    "graph",
    "workspace",
    "project",
    "integration",
    "reminder",
    "media",
    "capsule",
    "entity",
    "skill",
    "help",
    "generate_rules",
];

/// Common operations and query shortcuts.
/// This list is shared across all rule templates for consistency.
const SIMPLE_OPS_LIST: &str = r#"**Fast direct-read lane (no redundant grounding call):** after the mandatory first-session `init(...)` plus `context(...)`/`session(action="ground", ...)`, call these operations directly — without another `context(...)` or `ground` preamble — when the user's request is only that read and no state-changing tool has run since the last grounding:
- `workspace(action="list"|"get")`
- `memory(action="list_docs"|"list_events"|"list_todos"|"list_tasks"|"list_transcripts"|"list_nodes"|"list_diagrams", workspace_id="<current_workspace_id>", project_id="<current_project_id>")`
- `help(action="version"|"tools"|"auth")`
- `project(action="list"|"get"|"index_status")`
- `reminder(action="list"|"active")`

Do not use the direct-read lane for `recall`, decisions, searches, reading a specific document/event/task/transcript, media queries, or any create/update/delete/index operation. Those operations can depend on task-specific grounding or change state, so call `context(...)` first; if `context` is unavailable, call `session(action="ground", user_message="...")`.

**Common queries — use these exact tool calls:**
Every workspace-scoped call below must include `workspace_id="<current_workspace_id>"` using the exact value returned by `init(...)`/`context(...)`; also include `project_id="<current_project_id>"` when available.
- "list lessons" / "show lessons" → `session(action="get_lessons")`
- "save lesson" / "remember this lesson" / "lesson learned" / "I made a mistake" → `session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...", severity="low|medium|high|critical")` — **NEVER store lessons in local files** (e.g. `~/.claude/.../memory/`, `.cursorrules`, scratch markdown). Lessons live in ContextStream so they auto-surface as `[LESSONS_WARNING]` on future turns and across sessions.
- "list decisions" / "show decisions" / "how many decisions" → `memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when init/context surfaced ids; otherwise `memory(action="decisions")` after grounding/init
- "save decision" / "decided to" → `session(action="capture", event_type="decision", title="...", content="...")`
- "list docs" → `memory(action="list_docs")`
- "list tasks" → `memory(action="list_tasks", workspace_id="<current_workspace_id>", project_id="<current_project_id>")`
- "list todos" → `memory(action="list_todos")`
- "list plans" → `session(action="list_plans", workspace_id="<current_workspace_id>", project_id="<current_project_id>")`
- "save plan" / "capture plan" / "store plan" → `session(action="capture_plan", title="...", description="...", goals=[...], steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, acceptance criteria, verification"}], create_tasks=true, workspace_id="<current_workspace_id>", project_id="<current_project_id>")` — **NEVER** save plans with `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`
- "list events" → `memory(action="list_events")`
- "show snapshots" / "list snapshots" → `memory(action="list_events", event_type="session_snapshot")`
- "save snapshot" → `session(action="capture", event_type="session_snapshot", title="...", content="...")`
- "what did we do last session" / "past sessions" / "previous work" / "pick up where we left off" → `session(action="recall", query="...")` (ranked context) OR `memory(action="list_transcripts", limit=10)` (chronological list)
- "search past sessions" / "find in past transcripts" / "when did we discuss X" → `memory(action="search_transcripts", query="...")` — full-text search over saved conversation transcripts
- "show transcript" / "read session <id>" → `memory(action="get_transcript", transcript_id="...")`
- "list media" / "show assets" / "show photos/videos/audio/docs" → `media(action="list", content_types=["image"])` (use `image|video|audio|document`; omit `content_types` for all assets)
- "find media" / "search photos/videos/audio/docs" / "what's in this PDF/video/audio?" → `media(action="search", query="...", content_types=["document"])` (use `image|video|audio|document` as needed)
- "index media" / "upload asset" / "read this photo/video/audio/PDF" → `media(action="index", file_path="...", content_type="image")` or `media(action="index", external_url="...", content_type="document")`; use `image`, `video`, `audio`, or `document`, then check `media(action="status", content_id="...")`
- "extract clip" / "trim video" / "clip audio" → `media(action="get_clip", content_id="...", start="1:34", end="2:15", output_format="raw")` (also supports `ffmpeg` and `remotion`)
- "create capsule" / "context capsule" / "capsule link" → `capsule(action="create", scope="session", session_id="<current session id>", purpose="handoff")`. Session creates automatically mint a safe external-agent share and return the capsule id plus Agent URL and Dashboard URL; use `audience="self"` only when the user explicitly asks for no share link. **NEVER substitute a handoff entity, design spec, document, or prose summary for a requested capsule.**
- "share this context with another agent" / "handoff with a link" → first create the durable handoff with `entity(kind="handoff", action="create", body={...}, workspace_id="<current_workspace_id>", project_id="<current_project_id>")`, then create the portable artifact with `capsule(action="create", scope="session", session_id="<current session id>", purpose="handoff", workspace_id="<current_workspace_id>", project_id="<current_project_id>")`. Return both results.
- "create diagram" / "save diagram" / "show diagrams" → `memory(action="create_diagram", diagram_type="flowchart|sequence|class|er|gantt|mindmap|pie|other", title="...", content="...")` or `memory(action="list_diagrams")`; use `sequence` for service/API handoffs, `er` for data models, `flowchart` for process flows.
- "list skills" / "show my skills" → `skill(action="list")`
- "create a skill" → `skill(action="create", name="...", instruction_body="...", project_id="<current_project_id>", trigger_patterns=[...])`
- "update a skill" → `skill(action="update", name="...", instruction_body="...", change_summary="...")`
- "run skill" / "use skill" → `skill(action="run", name="...")`
- "import skills" / "import my CLAUDE.md" → `skill(action="import", file_path="...", format="auto")`

**Structured-entity queries (Phase 1-3 taxonomy expansion) — use the `entity` tool:**
- "create ticket" / "file bug" / "track feature" / "log incident" → `entity(kind="ticket", action="create", body={"title": "...", "kind": "bug|feature|task|chore|incident|epic", "priority": "low|medium|high|urgent"})`
- "list tickets" / "show open bugs" / "active features" → `entity(kind="ticket", action="list", query={"status": "open", "kind": "bug"})`
- "update ticket" / "close ticket" / "resolve bug" → `entity(kind="ticket", action="update", id="...", body={"status": "resolved"})`
- "create handoff" / "prepare a handoff" / "hand this over" / "continue with another agent/session" → `entity(kind="handoff", action="create", body={"title": "...", "summary": "...", "scope": "...", "next_steps": [...]})`. This is the default for every agent/session handoff; add `to_user_id` only when the recipient is known and never invent it. If the user requests a portable bundle, capsule, or share link, additionally call `capsule(...)`. **NEVER substitute `HANDOFF.md`, a scratch prompt, a generic doc/event, or a prose-only response for the ContextStream handoff.**
- "list handoffs" / "pending handoffs for me" → `entity(kind="handoff", action="list", query={"to_user_id": "<me>", "status": "pending"})`
- "share this with the other project" / "coordination inbox" / "ack coordination" → `coordination(action="inbox"|"share"|"ack")`. Distinct from handoffs. When `[COORDINATION]` appears, read it before continuing and ack after using it via `coordination(action="ack", notice_id="...")`.
- "log incident" / "open incident" / "sev1" → `entity(kind="incident", action="create", body={"title": "...", "severity": "sev1|sev2|sev3|sev4", "status": "detected", "services_affected": ["..."]})`
- "list incidents" / "active incidents" → `entity(kind="incident", action="list", query={"status": "investigating"})`
- "create release" / "track release" / "deployment" → `entity(kind="release", action="create", body={"version": "1.4.0", "status": "planned", "environments": ["prod"], "git_ref": "..."})`
- "list releases" / "recent deploys" → `entity(kind="release", action="list", query={"status": "released"})`
- "create experiment" / "start A/B test" → `entity(kind="experiment", action="create", body={"name": "...", "hypothesis": "...", "control": "...", "treatment": "...", "primary_metric": "..."})`
- "list experiments" / "running A/B tests" → `entity(kind="experiment", action="list", query={"status": "running"})`
- "create goal" / "new OKR" / "objective" → `entity(kind="goal", action="create", body={"objective": "...", "period": "2026-Q2", "owner_user_id": "..."})`
- "list goals" / "OKRs this quarter" → `entity(kind="goal", action="list", query={"period": "2026-Q2", "status": "active"})`
- "add key result" / "track KR progress" → `entity(kind="key_result", action="create", body={"goal_id": "<uuid>", "title": "MAU > 10k", "unit": "number", "target_value": 10000, "current_value": 6500})`
- "create sprint" / "new iteration" → `entity(kind="sprint", action="create", body={"name": "Sprint 42", "starts_at": "...", "ends_at": "...", "goal": "..."})`
- "list sprints" / "active sprint" → `entity(kind="sprint", action="list", query={"status": "active"})`
- "request review" / "PR review" / "design review" → `entity(kind="review", action="create", body={"title": "...", "kind": "pr|code|design|security|architecture|product", "subject_ref": "github:org/repo#123", "reviewer_ids": [...]})`
- "list reviews" / "pending reviews" → `entity(kind="review", action="list", query={"status": "requested"})`
- "log risk" / "track risk" / "risk register" → `entity(kind="risk", action="create", body={"title": "...", "likelihood": "possible", "impact": "major", "category": "...", "mitigation": "..."})`
- "list risks" / "open risks" / "severe risks" → `entity(kind="risk", action="list", query={"status": "open", "impact": "severe"})`
- "create backlog view" / "save backlog filter" → `entity(kind="backlog_view", action="create", body={"name": "Now/Next/Later", "bucket": "now", "filters": {...}})`
- "save runbook" / "create runbook" → `memory(action="create_doc", doc_type="runbook", title="...", content="...")` (plus 20 other doc types: adr, rfc, postmortem, retro, release_notes, playbook, prd, user_story, persona, interview, design_spec, critique, glossary, oncall_schedule, slo, q_and_a, changelog, style_guide)
- "save goal node" / "distill OKR" → `memory(action="create_node", node_type="goal"|"risk"|"term", summary="...", details="...")`
- "log standup" / "log status" / "log feedback" / "log achievement" → `memory(action="create_event", event_type="standup"|"status_update"|"feedback"|"achievement"|"discovery"|"question"|"approval", title="...", content="...")`

Use `context(user_message="...", mode="fast")` for quick turns.
Use `context(user_message="...")` for deeper analysis and coding tasks.
Match context depth to effort: `mode="fast"` for low/medium-effort lookups; `mode="pack"` or standard for high/xhigh/max deep work. With adaptive, interleaved thinking (e.g. Claude Opus 4.8) you reason *between* tool calls — so think, call `context()`, then `search()`, then act, rather than front-loading one call.
If the `instruct` tool is available, run `instruct(action="get", session_id="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` before `context(...)` on each turn, then `instruct(action="ack", session_id="...", workspace_id="<current_workspace_id>", ids=[...])` after using entries. Reuse the ids returned by init/context; if no current project is resolved, omit project_id intentionally for workspace-only instructions rather than inferring it.

**Plan-mode guardrail:** Entering plan mode does NOT bypass search-first. Do NOT use Explore, Task subagents, Grep, Glob, Find, SemanticSearch, `code_search`, `grep_search`, `find_by_name`, or shell search commands (`grep`, `find`, `rg`, `fd`). Start with `search(mode="auto", query="...")` — it handles glob patterns, regex, exact text, file paths, and semantic queries. Only Read narrowed files/line ranges returned by search."#;

/// Canonical handoff policy shared by every generated rules mode and the
/// managed Copilot skill. The compact harness contract carries the same
/// semantics into lifecycle/help/hook surfaces.
const HANDOFF_GUIDANCE: &str = r#"## Canonical Agent Handoffs

When the user asks to prepare/create a handoff, hand work over, or continue in another agent/session, create the handoff in ContextStream immediately:

`entity(kind="handoff", action="create", body={"title":"...","summary":"...","scope":"...","next_steps":[...]}, workspace_id="<current_workspace_id>", project_id="<current_project_id>")`

- Preserve concrete state: verified facts, eliminated hypotheses, branch/commit status, environment gotchas, validation already run, blockers, and ordered next steps.
- Add `to_user_id` only when the recipient is known; never invent a recipient. Reuse the current `workspace_id` and `project_id` when available.
- If a portable bundle, capsule, or share link is requested, **also** call `capsule(action="create", scope="session", session_id="<current session id>", purpose="handoff")` and return both the handoff and capsule links.
- A local `HANDOFF.md`, scratch prompt, generic document/event, or prose-only summary is **not** the canonical handoff and is never a substitute for the tool call.
- If the user explicitly requests a local handoff file, create the ContextStream handoff first, then write the exact requested file as an additional artifact.
- If the user explicitly requests a capsule, create a real capsule; never replace that request with only a handoff entity or prose.
"#;

/// Canonical plan/task persistence policy shared across generated rules.
const PLANS_AND_TASKS_GUIDANCE: &str = r#"## Plans and Tasks

**ALWAYS** use ContextStream for plans and tasks — do NOT create markdown plan files, use built-in todo/plan tools, or save plans as generic events.

**Do NOT save plans this way:**
- `session(action="capture", event_type="plan", ...)`
- `memory(action="create_event", event_type="plan", ...)`
- local `plan.md`, `.windsurf/plans`, `.cursor/plans`, `TodoWrite`, `todo_list`, or `plan_mode_respond` as the durable record

**Save comprehensive plans with the plan API:**
```
session(action="capture_plan",
  title="...",
  description="scope, constraints, affected areas, acceptance criteria, verification strategy",
  goals=["clear success criterion", "..."],
  steps=[
    {"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, files/modules if known, acceptance criteria, verification"}
  ],
  create_tasks=true,
  workspace_id="<current_workspace_id>",
  project_id="<current_project_id>")
```

Plan step descriptions must be detailed enough for a fresh agent to execute without re-asking: include scope, concrete work, affected files/modules if known, acceptance criteria, verification/test commands, and risks or rollback notes when relevant.

`capture_plan` creates one linked task per step by default. If tasks are created manually, every plan task must include:
```
memory(action="create_task",
  title="...",
  description="concrete work, acceptance criteria, verification",
  plan_id="<plan uuid>",
  plan_step_id="plan-step-1",
  priority="medium",
  task_status="pending",
  workspace_id="<current_workspace_id>",
  project_id="<current_project_id>")
```

After saving a plan, verify it is retrievable with `session(action="get_plan", plan_id="<plan uuid>", include_tasks=true, workspace_id="<current_workspace_id>", project_id="<current_project_id>")` or `session(action="list_plans", query="...", include_tasks=true, workspace_id="<current_workspace_id>", project_id="<current_project_id>")`."#;

/// Shared guidance to stop trial-and-error when ContextStream already has help.
const KNOWLEDGE_FIRST_GUIDANCE: &str = r#"## Finding Information — Search ContextStream Knowledge, Not Just Code

**Auto-grounding:** Every `context(user_message="...")` call may include a `[GROUNDING]` block — pre-ranked prior work (transcripts, snapshots, docs, decisions, lessons) for **this** message. When you see it, read those hits **before** fanning out into code search; skipping search entirely is often correct. Outside `context()`, use `session(action="ground", user_message="...")` for the same one-shot bundle (recall + docs + decisions + lessons + skills + git).

### Freshness Before Assumptions

Grounding and memory are evidence, not permission to use stale facts as current truth. Before planning or implementing from prior work, inspect the hit kind and age:
- **Decisions, transcript continuity, session snapshots, active plans, and tasks are time-sensitive.** Prefer recent hits. If a hit is marked stale, older than the local freshness window, or conflicts with newer context, refresh with `session(action="ground", user_message="...")`, `memory(action="decisions", query="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available, or `memory(action="search_transcripts", query="...")` before relying on it.
- **Lessons and preferences are durable but still age-stamped.** Follow them unless superseded, contradicted by newer surfaced context, or explicitly corrected by the user.
- **Docs and runbooks are authoritative unless superseded.** If a doc/runbook has operational facts that may drift (regions, hosts, credentials, deploy paths), verify through the referenced source or a fresh ContextStream lookup before acting.
- **LLM/Gemini-derived insights are advisory until captured as decisions.** Use `[INSIGHT]` or synthesized context to guide investigation, but do not treat it as a durable decision unless it is backed by a current decision/event/doc source.

### Checkout Currency Before Production Diagnosis

Before using local source to explain current production behavior, fetch the upstream/deployed ref and compare it to local `HEAD`. If they differ, do not call the checkout "fine" or treat its files as current until every inspected path is proven identical with a path-scoped comparison. Otherwise read the exact upstream blobs or use a separate clean checkout at the deployed/latest ref. Never pull, reset, rebase, or overwrite a dirty checkout to make it current; preserve user changes and diagnose from a clean clone or worktree.

When you need information, do not default to code search or trial-and-error. ContextStream stores far more than source — docs, decisions, lessons, preferences, plans, tasks, todos, skills, memory nodes, and full session transcripts all live behind dedicated tools. Pick the right knowledge surface by what you're looking for:

- **Source code / symbol / file** → `search(mode="auto", query="...")`
- **Why we did X / past decisions** → `memory(action="decisions", query="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available
- **Architecture / spec / design doc** → `memory(action="list_docs")` then `memory(action="get_doc", doc_id="title or UUID")`
- **Prior mistakes ("never do X again")** → `session(action="get_lessons", query="...")`
- **User preferences / conventions / constraints** → already surfaced as `[PREFERENCE]`; also `memory(action="list_nodes", node_type="preference")` or `memory(action="list_nodes", node_type="constraint")`
- **Open work / tasks / todos** → `memory(action="list_tasks")` / `memory(action="list_todos")`
- **Active or past plans** → `session(action="list_plans")` then `session(action="get_plan", plan_id="...")`
- **Reusable workflows / skills** → `skill(action="list")` then `skill(action="run", name="...")`
- **Diagrams / Mermaid-style architecture maps** → `memory(action="create_diagram", diagram_type="flowchart|sequence|class|er|gantt|mindmap|pie|other", title="...", content="...")`; diagram types are first-class and queryable with `memory(action="list_diagrams")`
- **Media assets (photos/images, video, audio, documents/PDFs)** → `media(action="search", query="...", content_types=["image"])`, `media(action="list")`, or `media(action="status", content_id="...")`. Use `image`, `video`, `audio`, or `document` in `content_types`. To make a local/URL asset readable by ContextStream, use `media(action="index", file_path="...", content_type="image")`; friendly words like photos/images map to `image`, docs/PDFs/slides map to `document`.
- **Tickets / bugs / features / chores / incidents / epics** → `entity(kind="ticket", action="list", query={...})` then `entity(kind="ticket", action="get", id="...")`
- **Handoffs (context bundles between sessions/agents/teammates)** → `entity(kind="handoff", action="list")` — pair with `capsule(...)` for the artefact bundle
- **Coordination (live shared awareness + durable items across workspaces/projects)** → `coordination(action="inbox"|"share"|"ack")`. Not a handoff. When `[COORDINATION]` appears, read it before continuing and ack after use.
- **Incidents (severity + status timeline)** → `entity(kind="incident", action="list")` — distinct from `EventType::Incident` raw events
- **Releases (versioned deploys)** → `entity(kind="release", action="list")` — `changelog_doc_id` links to a `doc_type='release_notes'` doc
- **Experiments / A/B tests** → `entity(kind="experiment", action="list")`
- **Goals / OKRs / key results** → `entity(kind="goal", action="list")`, then `entity(kind="key_result", action="list")` per goal
- **Sprints / iterations** → `entity(kind="sprint", action="list", query={"active_at": "<now>"})`
- **Reviews (PR / code / design / security / architecture / product)** → `entity(kind="review", action="list")`
- **Risks (active risk register)** → `entity(kind="risk", action="list")` — distinct from distilled `node_type='risk'` summary nodes
- **Runbooks / ADRs / RFCs / postmortems / retros / release-notes / playbooks / PRDs / personas / glossary / SLOs / etc.** → `memory(action="list_docs", doc_type="runbook|adr|rfc|postmortem|retro|release_notes|playbook|prd|user_story|persona|interview|design_spec|critique|glossary|oncall_schedule|slo|q_and_a|changelog|style_guide")`
- **"What did we do before?" (continuation work)** → read fresh `[GROUNDING]` from `context()` first; use `session(action="recall", query="...")` only when that grounding is insufficient — see the Past Sessions ladder below
- **Unsure which surface** → `memory(action="search", query="...")` — hybrid across memory nodes + docs; falls back to `session(action="recall", query="...")` for transcript/snapshot coverage

Default assumption: if the user asks "how do we do X?", "why did we choose Y?", "what's the pattern for Z?", or "did we already decide about Q?" — the answer is likely in a doc, decision, lesson, plan, or skill, NOT in the code. Check the right knowledge surface BEFORE reading source files, re-deriving the answer, or asking the user a clarifying question.

⚠️ **Don't re-ask what you just read.** A common failure mode: you find a runbook/doc/ticket/decision that records a fact (which DB? which region? which env? when's the deadline? which team owns X?), then still ask the user "is this correct?" or "is this still current?". That's a wasted turn — treat surfaced knowledge as the current truth unless you have a specific reason to suspect it's stale (commit history says it changed, the user explicitly contradicts it, etc.). When in doubt about staleness, verify by reading the **referenced source** (`git log` on the file, the cited code, the linked dashboard) — not by re-asking the user.

Clarifying-question budget: before asking the user *anything* a project artefact could answer, do one quick pass through `context()`/`ground()` hits, runbooks, decisions, transcripts, and entity records (tickets/handoffs/releases). If after that the answer is genuinely missing or ambiguous, then ask — and make the question specific ("the runbook from 2026-04-30 says Crunchy Bridge — is that still current as of today?" beats "where is prod running?").

Before guessing, improvising, or struggling through a workflow you don't fully know:
- Start with `context(...)` when that tool is exposed, or `session(action="ground", user_message="...")` when `context` is unavailable, and obey `[GROUNDING]` (prior-work anchors), `[MATCHED_SKILLS]`, `[LESSONS_WARNING]`, `[PREFERENCE]`, `[DECISIONS]`, `[MEMORY]`, and `<system-reminder>` output — those are already filtered to the current task
- Treat `[LESSONS_WARNING]` as active working instructions for the current task, not optional background context; apply them immediately and keep them in mind until the task is done
- Prefer surfaced ContextStream knowledge over inventing a new workflow from memory
- Prefer surfaced ContextStream knowledge over asking the user — clarifying questions are a last resort, not a first reflex
"#;

/// Past-session awareness — make it IMPOSSIBLE to miss that prior
/// conversations are queryable. This is a first-class capability: every
/// exchange is captured as a transcript and indexed for full-text search,
/// and `session(action="capture", event_type="session_snapshot", ...)`
/// bookmarks turning points.
const PAST_SESSIONS_GUIDANCE: &str = r#"## Past Sessions Are Queryable — USE THEM

### Auto-Grounding (in `context()`)

When `context()` returns `[GROUNDING]`, those lines are **pre-ranked prior work for your current message** — read them first (transcript/snapshot/doc/decision/lesson entry points). If the grounding is fresh, relevant, and sufficient to continue, it completes the continuation-retrieval step: **do not immediately call `session(action="recall")` for the same request.** Skipping both duplicate recall and code search is often correct. For the same bundle **outside** `context()`, call `session(action="ground", user_message="...")`.

Freshness matters: when grounding includes old decisions, transcript continuity, snapshots, plans, or tasks, refresh before using them to choose an implementation path. Recent decisions beat older decisions; superseded or stale hits are leads to verify, not assumptions to carry forward.

Transcripts for every turn of every session are captured and indexed automatically. Session snapshots bookmark turning points. **Before asking the user what you did last time, or re-deriving context you built together previously, check the transcript + snapshot layer.** It's fast, it's complete, and the user is paying for it.

Triggers to query past sessions:
- User says "last time", "previous", "yesterday", "earlier", "we decided", "we talked about", "pick up where we left off", "what were we working on"
- You have a task that's clearly a continuation (e.g. finishing a refactor that's half-done on disk)
- You're about to ask a clarifying question whose answer is likely in a prior session
- You're unsure whether a decision or approach has already been made

Continuation-retrieval ladder — walk it in order and stop at the first step that answers the question:

0. **Fresh `[GROUNDING]` from `context()` (or `session(action="ground", ...)`)** — this is the first retrieval. If it is relevant and sufficient, stop here. Do not duplicate it with an immediate recall call.

1. **`session(action="recall", query="<what you're continuing>")`** — the first explicit escalation when `[GROUNDING]` is absent, thin, stale, off-topic, or when the user explicitly requests broader or session-specific history. Ranked fusion across transcripts, snapshots, docs, and decisions.

2. **`memory(action="search_transcripts", query="<keyword or phrase>")`** — fall through when `recall` returns thin or off-topic results, or when you need every mention of a specific term. Full-text search across ALL saved transcripts.

3. **`memory(action="list_events", event_type="session_snapshot")`** — when you want the turning-point bookmarks (manual + auto pre-compaction captures). Useful for "what state were we in at the end of <session>" questions that `recall` misses because the answer isn't in conversational text.

4. **`memory(action="list_transcripts", limit=10)`** — when you need a chronological index of recent sessions (titles, timestamps, IDs). Use when the user wants to know "when did we last work on X".

5. **`memory(action="get_transcript", transcript_id="<uuid>")`** — read a full past session end-to-end. Use only after the steps above pointed you at a specific transcript ID and you need the complete exchange, not snippets.

6. **End of current session — save a bookmark** for the next one: `session(action="capture", event_type="session_snapshot", title="...", content="<what we did + next step>")`.

**Never answer "I don't know what we did before" without first inspecting `[GROUNDING]`; when it is insufficient, run step 1, then step 2 if recall is thin.**
"#;

/// Shared guidance to keep project-scoped writes attached to the right project.
const PROJECT_SCOPE_GUIDANCE: &str = r#"## Project Scope Discipline

- **`workspace_id` is required for every workspace-scoped ContextStream call.** After `init(...)` or `context(...)` returns it, pass that exact value explicitly on every `memory(...)`, `session(...)`, `entity(...)`, plan, task, todo, doc, and handoff call; do not rely on implicit session scope.
- Every task operation must include `workspace_id`, including `memory(action="create_task"|"get_task"|"update_task"|"delete_task"|"list_tasks"|"reorder_tasks", ...)` and the dedicated `memory_create_task(...)` / `memory_update_task(...)` tools. A `task_id` does not replace workspace scope.
- Reuse the `project_id` returned by `init(...)` or `context(...)` for project-scoped writes and lookups
- Reuse the `workspace_id` returned by `init(...)` or `context(...)` for workspace-scoped reads such as `memory(action="decisions")`; pass both `workspace_id` and `project_id` when both ids are available
- For project-scoped `memory(...)`, `session(...)`, and `skill(...)` calls, pass explicit `workspace_id` and `project_id` instead of guessing from the folder name or title
- When `[PROJECT_ROUTING]` appears with `uncertain`, `ambiguous`, `needs_project_selection`, or `needs_project_setup`, resolve scope before project-scoped work: choose a surfaced candidate, pass explicit `workspace_id`/`project_id`, or rerun `init(folder_path="...")` / `context(folder_path="...")`
- If `init(...)` or `context(...)` does not surface a current `project_id`, rerun `init(folder_path="...")` before creating docs, skills, events, tasks, todos, or other project memory
- Use `target_project` only after init from a multi-project parent folder
"#;

/// Shared guidance for Code Health / graph quality workflows.
const GRAPH_QUALITY_GUIDANCE: &str = r#"## Code Health and Dependency Recommendations

When the user asks about code quality, dependency risk, circular dependencies, unused code, complexity, dashboard scans, or whether prior dashboard analysis can guide work, use the `graph` tool before guessing from source alone:

- Dashboard freshness/cache state → `graph(action="quality_freshness", project_id="...")`
- Trend counts over time → `graph(action="quality_trends", project_id="...", limit=30)`
- Saved scan/run lifecycle → `graph(action="quality_history", project_id="...", limit=18)`
- Circular dependencies → `graph(action="circular_dependencies", project_id="...", limit=50)`
- Unused code → `graph(action="unused_code", project_id="...", limit=200, element_type="Function|Type|Module|Variable")`
- Complexity and long functions → `graph(action="complexity_metrics", project_id="...", limit=20)`
- Module/function dependency blast radius → `graph(action="dependencies", target_type="module|function|type|variable", target_id="...")`
- Save a fresh dashboard baseline after scans/fixes → `graph(action="quality_snapshot", project_id="...")`

Use the returned `recommendations` field and text summary to propose next steps. If results show non-zero cycles, unused code, complexity, regressions, or missing caches, recommend a small tracked plan/ticket set before editing. If results are clean, mention the clean baseline and suggest recording/refreshing snapshots only when useful.
"#;

/// Rules generation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
#[derive(Default)]
pub enum RulesMode {
    /// Minimal rules (~15 lines).
    #[default]
    Bootstrap,
    /// Standard rules (~80 lines).
    Minimal,
    /// Full rules with no-hooks supplement.
    Full,
}

fn mode_for_editor(editor: &Editor) -> RulesMode {
    match editor.enforcement_tier() {
        super::editors::EnforcementTier::TierA => RulesMode::Bootstrap,
        super::editors::EnforcementTier::TierB => RulesMode::Minimal,
        super::editors::EnforcementTier::TierC => RulesMode::Full,
    }
}

fn ensure_windsurf_always_on_frontmatter(content: &str) -> String {
    if content.starts_with(WINDSURF_ALWAYS_ON_FRONTMATTER) {
        return content.to_string();
    }
    format!("{}{}", WINDSURF_ALWAYS_ON_FRONTMATTER, content)
}

/// Resolve a rules target path to an actual file path.
///
/// Some editors may use directory-style rule locations (for example, `.clinerules/`).
/// When a directory is provided, write/read the managed block in
/// `<dir>/contextstream.md`.
fn resolve_rules_file_path(path: &Path) -> PathBuf {
    if path.exists() && path.is_dir() {
        path.join("contextstream.md")
    } else {
        path.to_path_buf()
    }
}

fn write_contextstream_block_to_path(
    path: &Path,
    new_block: &str,
    create_if_missing: bool,
) -> Result<bool> {
    let resolved_path = resolve_rules_file_path(path);
    let allow_create = create_if_missing || (path.exists() && path.is_dir());
    if !resolved_path.exists() && !allow_create {
        return Ok(false);
    }

    let existing = match std::fs::read_to_string(&resolved_path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_create => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let existing_content = existing.as_deref().unwrap_or("");
    let stamped_block = stamp_block_with_canonical_rules_hash(new_block);

    let new_content =
        render_contextstream_block_for_path(&resolved_path, existing_content, &stamped_block)?;
    safe_edit::write_if_unchanged(&resolved_path, &new_content, existing.as_deref())?;
    Ok(true)
}

fn render_contextstream_block_for_path(
    path: &Path,
    existing: &str,
    stamped_block: &str,
) -> Result<String> {
    let is_mdc = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mdc"));
    if !is_mdc {
        return replace_contextstream_block(existing, stamped_block);
    }

    // An existing Cursor rule may carry user-authored YAML fields. Preserve
    // that prefix byte-for-byte; only newly created/no-frontmatter files get
    // ContextStream's canonical always-on frontmatter.
    if let Some((frontmatter, body)) = split_leading_frontmatter(existing) {
        Ok(format!(
            "{}{}",
            frontmatter,
            replace_contextstream_block(body, stamped_block)?
        ))
    } else {
        Ok(format!(
            "{}{}",
            CURSOR_MDC_FRONTMATTER,
            replace_contextstream_block(existing, stamped_block)?
        ))
    }
}

/// Split a leading YAML frontmatter block (`---` … `---`) from its body while
/// preserving the prefix, including blank lines after the closing fence.
///
/// Only a frontmatter block anchored at the very start of the file is
/// recognized. A malformed opening fence is ordinary user content.
fn split_leading_frontmatter(content: &str) -> Option<(&str, &str)> {
    let first_newline = content.find('\n')?;
    if content[..first_newline].trim_end() != "---" {
        return None;
    }

    let rest = &content[first_newline + 1..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let mut body_start = first_newline + 1 + offset + line.len();
            while body_start < content.len()
                && matches!(content.as_bytes()[body_start], b'\r' | b'\n')
            {
                body_start += 1;
            }
            return Some((&content[..body_start], &content[body_start..]));
        }
        offset += line.len();
    }

    None
}

/// Splice the canonical teaching-bundle fingerprint into a managed block.
///
/// Every editor surface gets the same fingerprint. This is deliberately a
/// release-independent fingerprint of the bundled teaching templates, not a
/// digest of the rendered file: editor-specific formatting and workspace
/// identity must not make a freshly-written rules file look stale to the
/// process-global doctor/runtime checks.
///
/// If the block is malformed (no opening tag) we pass it through
/// untouched. The staleness check then degrades gracefully — readers
/// just see "no embedded hash → can't tell, assume stale on first
/// drift" rather than crashing.
fn stamp_block_with_canonical_rules_hash(block: &str) -> String {
    let cleaned = mcp_types::rules_hash::strip_hash_marker(block);
    let marker = mcp_types::rules_hash::format_hash_marker(canonical_rules_bundle_hash());

    let opening = CONTEXTSTREAM_START;
    let Some(start) = cleaned.find(opening) else {
        return cleaned;
    };
    let line_start = cleaned[..start].rfind('\n').map_or(0, |index| index + 1);
    let opening_prefix = &cleaned[line_start..start];
    let marker_prefix =
        if opening_prefix.chars().all(char::is_whitespace) || opening_prefix.trim() == "#" {
            opening_prefix
        } else {
            ""
        };
    let after_open = start + opening.len();
    // Skip a single trailing newline after the opening tag so the
    // marker sits on its own line, not glued to `<contextstream>`.
    let insert_at = if cleaned.as_bytes().get(after_open) == Some(&b'\n') {
        after_open + 1
    } else {
        after_open
    };

    let mut out = String::with_capacity(cleaned.len() + marker.len());
    out.push_str(&cleaned[..insert_at]);
    if insert_at == after_open {
        // No newline was present; preserve a clean line break before the marker.
        out.push('\n');
    }
    out.push_str(marker_prefix);
    out.push_str(&marker);
    out.push_str(&cleaned[insert_at..]);
    out
}

fn append_fingerprint_component(material: &mut Vec<u8>, label: &str, content: &str) {
    material.extend_from_slice(&(label.len() as u64).to_le_bytes());
    material.extend_from_slice(label.as_bytes());
    material.extend_from_slice(&(content.len() as u64).to_le_bytes());
    material.extend_from_slice(content.as_bytes());
}

fn global_rules_content(
    editor: &Editor,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
) -> String {
    if *editor == Editor::Aider {
        return aider_read_pointer_block("~/.contextstream/rules.md");
    }
    generate_rule_content(
        editor,
        workspace_id,
        workspace_name,
        mode_for_editor(editor),
    )
}

fn project_rules_content(
    editor: &Editor,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    project_name: Option<&str>,
) -> String {
    let mut rules = generate_rule_content(
        editor,
        workspace_id,
        workspace_name,
        mode_for_editor(editor),
    );
    if let Some(name) = project_name {
        rules = rules.replace("# Project: mcp", &format!("# Project: {}", name));
    }
    if *editor == Editor::Aider {
        return aider_read_pointer_block(SHARED_PROJECT_RULES_PATH);
    }
    if *editor == Editor::Antigravity {
        rules = rules.replace(
            CONTEXTSTREAM_END,
            &format!(
                "For comprehensive long-form rules, import `@./{}` where supported.\n{}",
                SHARED_PROJECT_RULES_PATH, CONTEXTSTREAM_END
            ),
        );
    }
    rules
}

fn compute_rules_bundle_hash(teaching_contract: &str) -> String {
    let mut material = Vec::new();
    append_fingerprint_component(
        &mut material,
        "fingerprint-contract",
        RULES_BUNDLE_FINGERPRINT_VERSION,
    );
    append_fingerprint_component(&mut material, "teaching-contract", teaching_contract);

    for editor in Editor::all() {
        append_fingerprint_component(
            &mut material,
            &format!("{}:global", editor.id()),
            &global_rules_content(editor, None, None),
        );
        append_fingerprint_component(
            &mut material,
            &format!("{}:project", editor.id()),
            &project_rules_content(editor, None, None, None),
        );
    }

    // These sidecars affect whether/how the managed teaching is loaded even
    // though they are outside the XML block itself. Folding them in ensures a
    // content-only change still invalidates the installed teaching bundle.
    append_fingerprint_component(
        &mut material,
        "shared-project-rules",
        &shared_rules_content(None, None, None),
    );
    append_fingerprint_component(&mut material, "cursor-frontmatter", CURSOR_MDC_FRONTMATTER);
    append_fingerprint_component(
        &mut material,
        "copilot-skill",
        &canonical_copilot_skill_content(),
    );

    mcp_types::rules_hash::fnv1a_64_hex(&material)
}

fn compute_canonical_rules_bundle_hash() -> String {
    compute_rules_bundle_hash(HARNESS_TEACHING_VERSION)
}

fn canonical_rules_bundle_hash() -> &'static str {
    CANONICAL_RULES_BUNDLE_HASH
        .get_or_init(compute_canonical_rules_bundle_hash)
        .as_str()
}

fn record_taught_evidence(editor: &Editor) {
    if safe_edit::is_dry_run() || !crate::readiness_evidence_writes_enabled() {
        return;
    }

    if let Err(error) = mcp_client::harness_readiness::record_taught(
        editor.harness_id(),
        HARNESS_TEACHING_VERSION,
        canonical_rules_bundle_hash(),
    ) {
        tracing::warn!(
            editor = editor.id(),
            error = %error,
            "Rules were written, but harness readiness evidence could not be recorded"
        );
    }
}

/// Initialize the process-global `canonical_rules_hash()` from this
/// binary's complete teaching bundle. Producers call this once during startup
/// so request bodies, doctor, runtime drift checks, file markers, and readiness
/// evidence all use exactly the same value.
pub fn install_canonical_rules_hash() {
    mcp_types::rules_hash::set_canonical_rules_hash(canonical_rules_bundle_hash());
}

fn shared_rules_content(
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    project_name: Option<&str>,
) -> String {
    let mut content = generate_rule_content(
        &Editor::Codex,
        workspace_id,
        workspace_name,
        RulesMode::Full,
    );
    if let Some(name) = project_name {
        content = content.replace("# Project: mcp", &format!("# Project: {}", name));
    }
    content
}

fn parse_workspace_identity_from_rules_content(content: &str) -> (Option<String>, Option<String>) {
    let workspace_name = content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("# Workspace: ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });

    let workspace_id = content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("# Workspace ID: ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });

    (workspace_id, workspace_name)
}

/// Return whether a delimited rules block is recognizably owned by
/// ContextStream.
///
/// The XML-style tags are intentionally human-readable and are not, by
/// themselves, an ownership claim: a user may legitimately use
/// `<contextstream>` in their own instructions. New blocks carry the explicit
/// content-hash marker. For backward compatibility, accept the old HTML
/// sentinels and the stable identity/header combination emitted by all
/// unmarked XML templates.
fn contextstream_rules_block_is_owned(block: &str) -> bool {
    if block.contains(RULES_HASH_MARKER_PREFIX)
        || block.contains(LEGACY_START)
        || block.contains(LEGACY_END)
    {
        return true;
    }

    let has_workspace_name = block
        .lines()
        .any(|line| line.trim_start().starts_with("# Workspace: "));
    let has_workspace_id = block
        .lines()
        .any(|line| line.trim_start().starts_with("# Workspace ID: "));
    let has_generated_header =
        block.contains("# ContextStream Rules") && block.contains("MANDATORY STARTUP:");

    has_workspace_name && has_workspace_id && has_generated_header
}

pub(crate) fn content_has_owned_contextstream_rules(content: &str) -> bool {
    let Some((start, end)) = find_contextstream_block_bounds(content) else {
        return false;
    };
    let block = &content[start..end];
    (block.contains(CONTEXTSTREAM_END) || block.contains(LEGACY_END))
        && contextstream_rules_block_is_owned(block)
}

fn is_placeholder_workspace_name(value: &str) -> bool {
    value == DEFAULT_WORKSPACE_NAME
}

fn is_placeholder_workspace_id(value: &str) -> bool {
    value == DEFAULT_WORKSPACE_ID
}

fn prefer_inferred_value(
    current: &mut Option<String>,
    candidate: Option<String>,
    is_placeholder: fn(&str) -> bool,
) {
    let Some(candidate) = candidate else {
        return;
    };

    let current_is_placeholder = current.as_deref().map(is_placeholder).unwrap_or(true);
    let candidate_is_placeholder = is_placeholder(&candidate);

    if current.is_none() || (current_is_placeholder && !candidate_is_placeholder) {
        *current = Some(candidate);
    }
}

pub(crate) fn infer_workspace_identity_from_existing_rules(
    editors: &[Editor],
    project_path: Option<&Path>,
    include_global: bool,
    include_project: bool,
) -> (Option<String>, Option<String>) {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    let mut push_candidate = |path: PathBuf| {
        if seen.insert(path.clone()) {
            candidates.push(path);
        }
    };

    if include_global {
        for editor in editors {
            for path in editor.all_rules_paths(None) {
                push_candidate(path);
            }
        }

        if let Some(home) = dirs::home_dir() {
            push_candidate(home.join(".contextstream").join("rules.md"));
        }
    }

    if include_project {
        if let Some(project_path) = project_path {
            for editor in editors {
                for path in editor.all_rules_cleanup_paths(Some(project_path)) {
                    push_candidate(path);
                }
            }

            push_candidate(project_path.join(SHARED_PROJECT_RULES_PATH));
        }
    }

    let mut workspace_id = None;
    let mut workspace_name = None;

    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((start, end)) = find_contextstream_block_bounds(&content) else {
            continue;
        };
        let block = &content[start..end];
        if !contextstream_rules_block_is_owned(block)
            || (!block.contains(CONTEXTSTREAM_END) && !block.contains(LEGACY_END))
        {
            continue;
        }

        let (candidate_id, candidate_name) = parse_workspace_identity_from_rules_content(block);
        prefer_inferred_value(&mut workspace_id, candidate_id, is_placeholder_workspace_id);
        prefer_inferred_value(
            &mut workspace_name,
            candidate_name,
            is_placeholder_workspace_name,
        );

        if workspace_id
            .as_deref()
            .is_some_and(|value| !is_placeholder_workspace_id(value))
            && workspace_name
                .as_deref()
                .is_some_and(|value| !is_placeholder_workspace_name(value))
        {
            break;
        }
    }

    (workspace_id, workspace_name)
}

fn write_shared_project_rules(
    project_path: &Path,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    project_name: Option<&str>,
) -> Result<PathBuf> {
    let shared_rules_path = project_path.join(SHARED_PROJECT_RULES_PATH);
    let shared = shared_rules_content(workspace_id, workspace_name, project_name);
    write_contextstream_block_to_path(&shared_rules_path, &shared, true)?;
    Ok(shared_rules_path)
}

fn build_copilot_skill_content() -> String {
    let base = r#"---
name: contextstream-workflow
description: "Manage persistent AI memory across sessions with ContextStream MCP."
---

<!-- ContextStream managed Copilot skill v2 sha256: 0000000000000000000000000000000000000000000000000000000000000000 -->

# ContextStream Workflow Skill

## Purpose

Use ContextStream to keep plans, tasks, decisions, lessons, and implementation context available across Copilot sessions.

## Session Lifecycle

### 1. Start the session

Always call `init` at the beginning of a new session:

```
init(
  folder_path="<project_path>",
  context_hint="<user's first message>"
)
```

Then call `context` with the current request:

```
context(
  user_message="<current user message>"
)
```

For later messages in the same session, call `context` first before doing more work.

Before inventing a workflow from memory, check whether ContextStream already surfaced relevant skills, docs, lessons, or decisions for the task.
Use `skill(action="list")`, `memory(action="list_docs")`, `session(action="get_lessons")`, and `memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available and the task is unfamiliar or likely already documented.
Reuse the current `project_id` returned by `init` or `context` for project-scoped docs, events, and skills instead of guessing.

### 2. Plan multi-step work

Capture a persistent plan:

```
session(
  action="capture_plan",
  title="Implement feature X",
  steps=[
    {"id": "1", "title": "Research the current code path", "order": 1},
    {"id": "2", "title": "Implement the change", "order": 2},
    {"id": "3", "title": "Add verification", "order": 3}
  ]
)
```

Then create linked tasks:

```
memory(
  action="create_task",
  title="Implement the change",
  plan_id="<plan_id>",
  plan_step_id="2",
  priority="high"
)
```

### 3. Track progress while working

Start a task:

```
memory(
  action="update_task",
  task_id="<task_id>",
  status="in_progress"
)
```

Capture a technical decision:

```
session(
  action="capture",
  event_type="decision",
  title="Use repository pattern for data access",
  content="Chose a repository layer to isolate persistence logic and simplify testing."
)
```

Finish a task:

```
memory(
  action="update_task",
  task_id="<task_id>",
  status="completed"
)
```

### 4. Capture lessons

When a mistake or correction happens, save a lesson immediately:

```
session(
  action="capture_lesson",
  title="Check pagination behavior before assuming full results",
  trigger="Assumed the API returned all records in one response",
  impact="Only the first page was processed",
  prevention="Verify pagination semantics before implementing the fetch path",
  severity="medium"
)
```

### 5. Finish the work

Update the plan:

```
session(
  action="update_plan",
  plan_id="<plan_id>",
  status="completed"
)
```

Capture a summary event:

```
memory(
  action="create_event",
  event_type="implementation",
  title="Feature X complete",
  content="Implemented the change, added tests, and verified the result."
)
```

## Search-First Workflow

- Before local code discovery, use `search(mode="auto", query="...")`
- Use `search(mode="keyword")` for exact symbols or strings
- Use `search(mode="pattern")` for glob or regex-style lookup
- Use local reads only after search narrows the file set
"#;
    format!("{}\n\n{}", base.trim_end(), HANDOFF_GUIDANCE.trim())
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn normalize_copilot_skill_line_endings(content: &str) -> Cow<'_, str> {
    if content.contains("\r\n") {
        Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

fn stamp_copilot_skill_hash(mut content: String) -> String {
    let hash_start = content
        .find(COPILOT_SKILL_HASH_MARKER_PREFIX)
        .expect("Copilot skill template must contain its managed hash marker")
        + COPILOT_SKILL_HASH_MARKER_PREFIX.len();
    let hash_end = hash_start + COPILOT_SKILL_HASH_PLACEHOLDER.len();
    assert_eq!(
        &content[hash_start..hash_end],
        COPILOT_SKILL_HASH_PLACEHOLDER,
        "Copilot skill template must contain the hash placeholder"
    );

    let fingerprint = sha256_hex(&[content.as_bytes()]);
    content.replace_range(hash_start..hash_end, &fingerprint);
    content
}

fn copilot_skill_hash_marker_is_valid(content: &str) -> bool {
    let normalized = normalize_copilot_skill_line_endings(content);
    let content = normalized.as_ref();
    let Some(marker_start) = content.find(COPILOT_SKILL_HASH_MARKER_PREFIX) else {
        return false;
    };
    let hash_start = marker_start + COPILOT_SKILL_HASH_MARKER_PREFIX.len();
    let Some(relative_hash_end) = content[hash_start..].find(COPILOT_SKILL_HASH_MARKER_SUFFIX)
    else {
        return false;
    };
    let hash_end = hash_start + relative_hash_end;
    let marker_end = hash_end + COPILOT_SKILL_HASH_MARKER_SUFFIX.len();
    let claimed_hash = &content[hash_start..hash_end];

    if claimed_hash.len() != COPILOT_SKILL_HASH_PLACEHOLDER.len()
        || !claimed_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || content[marker_end..].contains(COPILOT_SKILL_HASH_MARKER_PREFIX)
    {
        return false;
    }

    // The marker must be a standalone line. This prevents an otherwise valid
    // hash-looking fragment embedded in user prose from declaring ownership.
    let line_start = content[..marker_start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_end = content[marker_end..]
        .find('\n')
        .map_or(content.len(), |index| marker_end + index);
    if !content[line_start..marker_start].trim().is_empty()
        || !content[marker_end..line_end].trim().is_empty()
    {
        return false;
    }

    let actual_hash = sha256_hex(&[
        &content.as_bytes()[..hash_start],
        COPILOT_SKILL_HASH_PLACEHOLDER.as_bytes(),
        &content.as_bytes()[hash_end..],
    ]);
    claimed_hash.eq_ignore_ascii_case(&actual_hash)
}

fn write_copilot_skill_file(project_path: &Path) -> Result<()> {
    let skill_path = project_path.join(COPILOT_SKILL_PATH);
    let content = canonical_copilot_skill_content();
    let existing = match std::fs::read_to_string(&skill_path) {
        Ok(existing) => {
            if !copilot_skill_is_owned(&existing) {
                anyhow::bail!(
                    "Refusing to overwrite user-owned Copilot skill {}",
                    skill_path.display()
                );
            }
            Some(existing)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    safe_edit::write_if_unchanged(&skill_path, &content, existing.as_deref())?;
    Ok(())
}

fn canonical_copilot_skill_content() -> String {
    let mut content = build_copilot_skill_content().trim().to_string();
    content.push('\n');
    stamp_copilot_skill_hash(content)
}

fn copilot_skill_is_owned(content: &str) -> bool {
    let normalized = normalize_copilot_skill_line_endings(content);
    let normalized = normalized.as_ref();

    if normalized == canonical_copilot_skill_content()
        || copilot_skill_hash_marker_is_valid(normalized)
    {
        return true;
    }

    let fingerprint = sha256_hex(&[normalized.as_bytes()]);
    LEGACY_COPILOT_SKILL_SHA256.contains(&fingerprint.as_str())
}

fn aider_read_pointer_block(shared_rules_relative_path: &str) -> String {
    format!(
        "# {}\n# ContextStream managed rules reference\nread:\n  - {}\n# {}",
        CONTEXTSTREAM_START, shared_rules_relative_path, CONTEXTSTREAM_END
    )
}

/// Apply MCP tool name prefix for Claude Code.
/// Converts `init(`, `context(`, etc. to `mcp__contextstream__init(`, etc.
fn apply_mcp_prefix(content: &str) -> String {
    let mut result = content.to_string();
    for &tool in TOOL_NAMES {
        // Match tool_name( but not already prefixed
        let plain = format!("`{}(", tool);
        let prefixed = format!("`{}{}(", CLAUDE_MCP_PREFIX, tool);
        result = result.replace(&plain, &prefixed);

        // Match tool_name) in backtick contexts
        let plain_bt = format!("{}(", tool);
        let prefixed_bt = format!("{}{}(", CLAUDE_MCP_PREFIX, tool);
        // Only replace if not already prefixed
        result = result.replace(&prefixed_bt, &format!("__PLACEHOLDER__{}", tool));
        result = result.replace(&plain_bt, &prefixed_bt);
        result = result.replace(&format!("__PLACEHOLDER__{}", tool), &prefixed_bt);
    }
    result
}

/// Write editor rules file (global).
pub fn write_editor_rules(
    editor: &Editor,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
) -> Result<()> {
    let paths = editor.all_rules_paths(None);
    if paths.is_empty() {
        return Err(anyhow::anyhow!(
            "Could not determine rules path for {}",
            editor.display_name()
        ));
    }

    let rules = global_rules_content(editor, workspace_id, workspace_name);

    let primary = &paths[0];
    write_contextstream_block_to_path(primary, &rules, true)?;
    for legacy in paths.iter().skip(1) {
        let _ = write_contextstream_block_to_path(legacy, &rules, false)?;
    }

    // Keep a canonical global long-form rules file for Aider pointer-based loading.
    if *editor == Editor::Aider {
        if let Some(home) = dirs::home_dir() {
            let shared = home.join(".contextstream").join("rules.md");
            let shared_content = shared_rules_content(workspace_id, workspace_name, None);
            write_contextstream_block_to_path(&shared, &shared_content, true)?;
        }
    }

    record_taught_evidence(editor);
    Ok(())
}

/// Write project-level rules file.
///
/// `AGENTS.md` (used by Codex/OpenCode) is also loaded by Windsurf as additional
/// context, so when Windsurf already has ContextStream rules under
/// `.windsurf/rules/` we skip writing AGENTS.md to avoid the cross-editor
/// duplication that makes Windsurf ignore the rules entirely. Other editors
/// (Cursor, ClaudeCode, Cline, KiloCode, RooCode) read their own specific
/// rule files (Cursor reads `.cursor/rules/*.mdc`) and do NOT load AGENTS.md —
/// their presence must not trigger a skip, otherwise Codex/OpenCode end up with
/// no project rule context at all.
pub fn write_project_rules(
    editor: &Editor,
    project_path: &Path,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    project_name: Option<&str>,
) -> Result<()> {
    fn path_has_contextstream_markers(path: &Path) -> Result<bool> {
        let resolved = resolve_rules_file_path(path);
        if !resolved.try_exists().with_context(|| {
            format!(
                "Could not inspect Windsurf rules path {}",
                resolved.display()
            )
        })? {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&resolved).with_context(|| {
            format!(
                "Could not read Windsurf rules file {} before deciding whether to update AGENTS.md",
                resolved.display()
            )
        })?;
        Ok(content_has_owned_contextstream_rules(&content))
    }

    // Skip AGENTS.md (Codex/OpenCode) only when Windsurf has its own project
    // rules already. Windsurf is the one editor that loads BOTH its
    // `.windsurf/rules/` files and AGENTS.md, so duplicate ContextStream content
    // across the two makes Windsurf ignore them entirely. The other editors
    // (Cursor reads `.cursor/rules/*.mdc`, ClaudeCode reads `CLAUDE.md`,
    // Cline/KiloCode/RooCode read their own conventions) do not also consume
    // AGENTS.md and must not block its generation.
    if matches!(editor, Editor::Codex | Editor::OpenCode) {
        for windsurf_path in Editor::Windsurf.all_rules_paths(Some(project_path)) {
            if path_has_contextstream_markers(&windsurf_path)? {
                // If we're skipping AGENTS.md to avoid Windsurf duplication,
                // also clean any existing ContextStream block from AGENTS.md so
                // users don't have to manually remove stale legacy/mixed markers.
                if remove_contextstream_from_rules(editor, Some(project_path))? {
                    tracing::debug!("Removed stale ContextStream block from AGENTS.md before skip");
                }
                tracing::debug!(
                    "Skipping AGENTS.md rules — Windsurf already has ContextStream rules at {}",
                    windsurf_path.display()
                );
                return Ok(());
            }
        }
    }

    let paths = editor.all_rules_paths(Some(project_path));
    if paths.is_empty() {
        return Err(anyhow::anyhow!(
            "Could not determine rules path for {}",
            editor.display_name()
        ));
    }

    if *editor == Editor::Aider || *editor == Editor::Antigravity {
        let _ =
            write_shared_project_rules(project_path, workspace_id, workspace_name, project_name)?;
    }
    let rules = project_rules_content(editor, workspace_id, workspace_name, project_name);

    let primary = &paths[0];
    write_contextstream_block_to_path(primary, &rules, true)?;
    for legacy in paths.iter().skip(1) {
        let _ = write_contextstream_block_to_path(legacy, &rules, false)?;
    }

    if *editor == Editor::Copilot {
        write_copilot_skill_file(project_path)?;
    }

    // Cleanup legacy-only paths (e.g. Windsurf `.windsurfrules`) after writing
    // the authoritative project rules file.
    for cleanup_path in editor.legacy_cleanup_only_rules_paths(Some(project_path)) {
        if !cleanup_path.exists() {
            continue;
        }
        let _ = remove_contextstream_from_path(&cleanup_path)?;
    }

    record_taught_evidence(editor);
    Ok(())
}

/// Generate rule content for an editor.
pub fn generate_rule_content(
    editor: &Editor,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    mode: RulesMode,
) -> String {
    let ws_name = workspace_name.unwrap_or(DEFAULT_WORKSPACE_NAME);
    let ws_id = workspace_id.unwrap_or(DEFAULT_WORKSPACE_ID);

    let mut content = match mode {
        RulesMode::Bootstrap => generate_bootstrap_rules(editor, ws_name, ws_id),
        RulesMode::Minimal => generate_minimal_rules(editor, ws_name, ws_id),
        RulesMode::Full => generate_full_rules(editor, ws_name, ws_id),
    };

    // Append NO_HOOKS_SUPPLEMENT for editors without hooks
    let no_hooks = !editor.has_hooks();
    if no_hooks {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", NO_HOOKS_SUPPLEMENT, CONTEXTSTREAM_END),
        );

        // No-hook editors require a concrete default to avoid mode ambiguity.
        content = content
            .replace(
                "search(mode=\"...\", query=\"...\")",
                "search(mode=\"auto\", query=\"...\")",
            )
            .replace("search(mode=\"...\")", "search(mode=\"auto\")");
    }

    // Append editor-specific supplements
    if *editor == Editor::Windsurf {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", WINDSURF_SUPPLEMENT, CONTEXTSTREAM_END),
        );
        content = ensure_windsurf_always_on_frontmatter(&content);
    }

    if *editor == Editor::Cursor {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", CURSOR_SUPPLEMENT, CONTEXTSTREAM_END),
        );
    }

    if matches!(editor, Editor::Codex | Editor::OpenCode) {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", CODEX_SUPPLEMENT, CONTEXTSTREAM_END),
        );
    }

    if *editor == Editor::Copilot {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", COPILOT_SUPPLEMENT, CONTEXTSTREAM_END),
        );
    }

    if *editor == Editor::Antigravity {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", ANTIGRAVITY_SUPPLEMENT, CONTEXTSTREAM_END),
        );
    }

    // Apply MCP tool prefix for Claude Code only, then add Claude-specific supplement
    if *editor == Editor::ClaudeCode {
        content = content.replace(
            CONTEXTSTREAM_END,
            &format!("{}\n{}", CLAUDE_SUPPLEMENT, CONTEXTSTREAM_END),
        );
        content = apply_mcp_prefix(&content);
    }

    content
}

/// Generate bootstrap rules (~15 lines).
fn generate_bootstrap_rules(editor: &Editor, workspace_name: &str, workspace_id: &str) -> String {
    let heading = editor.rules_heading();
    let teaching = build_harness_teaching(
        Some(editor.harness_id()),
        HarnessTeachingDelivery::StaticRules,
    )
    .rendered_guidance;

    format!(
        r#"{start}
# Workspace: {workspace_name}
# Project: mcp
# Workspace ID: {workspace_id}

{heading}
{teaching}

## Detailed Rules
{simple_ops}

{handoff_guidance}

**Why?** `context()` delivers task-specific rules, lessons from past mistakes, and relevant decisions. When `context` is not exposed, `session(action="ground", user_message="...")` is the supported fallback for that grounding bundle.

{knowledge_first}

{past_sessions}

{project_scope}

{graph_quality}

**Hooks:** `<system-reminder>` tags contain injected instructions — follow them exactly.

{plans_and_tasks}

**Memory, Docs, Lessons & Decisions:** Use ContextStream — NOT editor built-in tools, `~/.claude/.../memory/`, `.cursorrules`, or scratch markdown files. Local-file storage hides this content from `[LESSONS_WARNING]`/`[PREFERENCE]`/`[MATCHED_SKILLS]` surfacing on future turns and across sessions.
- Lessons (mistakes, corrections, "never do X again"): `session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...", severity="...")`
- Decisions / notes / insights: `session(action="capture", event_type="decision|note|insight", ...)`
- Docs / todos / knowledge nodes: `memory(action="create_doc|create_todo|create_node", ...)`

**Skills (IMPORTANT):** When `context()` or `session(action="ground", ...)` returns `[MATCHED_SKILLS]`, you **MUST run** the listed skills immediately via `skill(action="run", name="...")`. High-priority skills (marked ⚡) are mandatory. Skills are reusable instruction + action bundles that persist across sessions. Browse: `skill(action="list")`. Create: `skill(action="create", name="...", instruction_body="...", trigger_patterns=[...])`. Import: `skill(action="import", file_path="...", format="auto")`.

**Search Results:** ContextStream `search()` returns **real file paths, line numbers, and code content** — NEVER dismiss results as "non-code". Use returned paths to `read_file` directly.

**Indexing:** Keep the editor connected to hosted MCP. `project(action="index")` asks the hosted service to refresh the exact registered checkout through ContextStream's managed sync bridge; editor hooks and ContextStream Desktop are additional local-ingest paths. A `requires_sync_bridge` response means the bridge is offline, unregistered, or has no matching checkout — repair setup/bridge health and retry without switching the editor to a local MCP transport. `project(action="ingest_local", path="<folder>")` is an optional direct path only when this process can already read the folder. Check `project(action="index_status")` for checkout-aware freshness.

**Notices:** [GROUNDING] → read ranked prior-work hits before code search and inspect freshness before relying on time-sensitive decisions/transcripts/plans | [GROUNDING_AVAILABLE] → hook reminder that unread grounding exists; inspect source age and refresh stale hits before planning or implementing | [PROJECT_ROUTING] → resolve ambiguous/missing project scope before project-scoped search, indexing, memory, session, skill, or capture writes | [MATCHED_SKILLS] → run surfaced skills before other work | [LESSONS_WARNING] → apply lessons immediately and keep them active for the turn | [COORDINATION] → read shared-awareness notices from other live agents before continuing; ack via `coordination(action="ack")`; do not treat as a handoff | [PREFERENCE] → follow user preferences | [RULES_NOTICE] → run `generate_rules()` | [VERSION_NOTICE/CRITICAL] → tell user about update
{end}
"#,
        start = CONTEXTSTREAM_START,
        end = CONTEXTSTREAM_END,
        heading = heading,
        teaching = teaching,
        workspace_name = workspace_name,
        workspace_id = workspace_id,
        simple_ops = SIMPLE_OPS_LIST,
        handoff_guidance = HANDOFF_GUIDANCE,
        knowledge_first = KNOWLEDGE_FIRST_GUIDANCE,
        past_sessions = PAST_SESSIONS_GUIDANCE,
        project_scope = PROJECT_SCOPE_GUIDANCE,
        graph_quality = GRAPH_QUALITY_GUIDANCE,
        plans_and_tasks = PLANS_AND_TASKS_GUIDANCE,
    )
}

/// Generate minimal rules (~80 lines).
fn generate_minimal_rules(editor: &Editor, workspace_name: &str, workspace_id: &str) -> String {
    let heading = editor.rules_heading();
    let teaching = build_harness_teaching(
        Some(editor.harness_id()),
        HarnessTeachingDelivery::StaticRules,
    )
    .rendered_guidance;

    format!(
        r#"{start}
# Workspace: {workspace_name}
# Project: mcp
# Workspace ID: {workspace_id}

{heading}
{teaching}

{simple_ops}

{handoff_guidance}

## Why These Rules?

- `context()` returns task-specific rules, lessons from past mistakes, and relevant decisions; when unavailable, `session(action="ground", user_message="...")` provides the supported grounding fallback
- `search()` uses semantic understanding to find relevant code faster than file scanning
- Default context-first keeps state reliable; the narrow read-only bypass avoids unnecessary repeats

{knowledge_first}

{past_sessions}

{project_scope}

{graph_quality}

## Response to Notices

- `[GROUNDING]` → Read ranked prior-work hits (from `context()`) before broad code search; inspect source age before relying on time-sensitive decisions, transcripts, snapshots, plans, or tasks; optional one-shot: `session(action="ground", user_message="...")`
- `[GROUNDING_AVAILABLE]` → Your editor may remind you when unread grounding exists; inspect freshness metadata and refresh stale hits before planning or implementation
- `[PROJECT_ROUTING]` → Resolve ambiguous or missing project scope before project-scoped search, indexing, memory, session, skill, or capture writes; choose a candidate, pass explicit ids, or rerun `init/context` with `folder_path`
- `[MATCHED_SKILLS]` → Run the surfaced skills before other work
- `[LESSONS_WARNING]` → Apply the lessons shown immediately and keep them active for the current task
- `[PREFERENCE]` → Follow user preferences exactly
- `[RULES_NOTICE]` → Run `generate_rules()` to update rules
- `[VERSION_NOTICE]` → Inform user about available updates

## System Reminders

`<system-reminder>` tags in messages contain injected instructions from hooks.
These should be followed exactly as they contain real-time context.

{plans_and_tasks}
{end}
"#,
        start = CONTEXTSTREAM_START,
        end = CONTEXTSTREAM_END,
        heading = heading,
        teaching = teaching,
        workspace_name = workspace_name,
        workspace_id = workspace_id,
        simple_ops = SIMPLE_OPS_LIST,
        handoff_guidance = HANDOFF_GUIDANCE,
        knowledge_first = KNOWLEDGE_FIRST_GUIDANCE,
        past_sessions = PAST_SESSIONS_GUIDANCE,
        project_scope = PROJECT_SCOPE_GUIDANCE,
        graph_quality = GRAPH_QUALITY_GUIDANCE,
        plans_and_tasks = PLANS_AND_TASKS_GUIDANCE,
    )
}

/// Generate full rules (minimal + expanded guidance).
fn generate_full_rules(editor: &Editor, workspace_name: &str, workspace_id: &str) -> String {
    let heading = editor.rules_heading();
    let teaching = build_harness_teaching(
        Some(editor.harness_id()),
        HarnessTeachingDelivery::StaticRules,
    )
    .rendered_guidance;

    format!(
        r#"{start}
# Workspace: {workspace_name}
# Project: mcp
# Workspace ID: {workspace_id}

{heading}
{teaching}

{simple_ops}

{handoff_guidance}

## Why These Rules?

- `context()` returns task-specific rules, lessons from past mistakes, and relevant decisions; when unavailable, `session(action="ground", user_message="...")` provides the supported grounding fallback
- `search()` uses semantic understanding to find relevant code faster than file scanning
- Transcript capture is optional and OFF by default. Enable per session with `save_exchange=true` (and `session_id`), disable with `save_exchange=false`.
- Default context-first keeps state reliable; the narrow read-only bypass avoids unnecessary repeats

{knowledge_first}

{past_sessions}

{project_scope}

{graph_quality}

## Response to Notices

- `[GROUNDING]` → Read ranked prior-work hits (from `context()`) before broad code search; inspect source age before relying on time-sensitive decisions, transcripts, snapshots, plans, or tasks; optional one-shot: `session(action="ground", user_message="...")`
- `[GROUNDING_AVAILABLE]` → Your editor may remind you when unread grounding exists; inspect freshness metadata and refresh stale hits before planning or implementation
- `[PROJECT_ROUTING]` → Resolve ambiguous or missing project scope before project-scoped search, indexing, memory, session, skill, or capture writes; choose a candidate, pass explicit ids, or rerun `init/context` with `folder_path`
- `[MATCHED_SKILLS]` → Run the surfaced skills before other work
- `[LESSONS_WARNING]` → Apply the lessons shown immediately and keep them active for the current task
- `[PREFERENCE]` → Follow user preferences exactly
- `[RULES_NOTICE]` → Run `generate_rules()` to update rules
- `[VERSION_NOTICE]` → Inform user about available updates

## System Reminders

`<system-reminder>` tags in messages contain injected instructions from hooks.
These should be followed exactly as they contain real-time context.

## Search Protocol

**IMPORTANT: Hosted MCP is the preferred editor transport. `project(action="index")` requests the managed sync bridge registered on the machine that owns the checkout; editor hooks and ContextStream Desktop can also ingest local changes. A `requires_sync_bridge` response means bridge/setup health needs repair — keep the editor on hosted MCP, repair the bridge or hooks, and retry instead of switching transports or retry-looping blindly.**

1. Check project index: `project(action="index_status")`
2. If indexed (fresh/recent/aging/stale): run `search(mode="...", query="...")` immediately before local tools. Do not wait for an instantly fresh index.
3. If index coverage is missing or first indexing is still starting: retry `search(mode="auto", ...)` after a short wait — keyword search returns committed results as the index builds, so retry rather than jumping to local tools
4. If search returns results with a stale-index advisory, treat those results as usable for existing indexed code; refresh in background and retry only before concluding a newly edited/created symbol is absent
5. If search returns 0 results after a targeted retry, or you are inspecting known-new local edits, local tools are allowed

### Search Mode Selection:
- `auto` (recommended): query-aware mode selection
- `hybrid`: mixed semantic + keyword retrieval for broad discovery
- `semantic`: conceptual/natural-language questions ("how does auth work?")
- `keyword`: exact text or quoted string
- `pattern`: glob/regex queries (`*.sql`, `foo\s+bar`)
- `refactor`: symbol usage / rename-safe lookup (`UserService`, `snake_case`)
- `exhaustive`: all occurrences / complete match sets
- `team`: cross-project team search

### Output Format Hints:
- `output_format="paths"` for file lists and rename targets
- `output_format="count"` for "how many" queries

### Two-Phase Search Playbook (recommended):
1. **Discovery pass**: run `search(mode="auto", query="<concept + module>", output_format="paths", limit=10)`
2. **Precision pass**: use symbols from pass 1 with a specific mode:
   - Exact symbol/text: `search(mode="keyword", query="\"my_symbol\"", include_content=true, file_types=["rs"], limit=20)`
   - Symbol usage/rename-safe lookup: `search(mode="refactor", query="MySymbol", output_format="paths")`
   - Complete usage sweep: `search(mode="exhaustive", query="my_symbol", file_types=["rs"])`
3. **Read locally only after narrowing**: use Read/Grep on returned paths, not the full repo.

{plans_and_tasks}

## Memory, Docs & Todos

**ALWAYS** use ContextStream for memory, lessons, decisions, documents, and todos — NOT editor built-in tools, `~/.claude/.../memory/`, `.cursorrules`, or local files. Local-file storage is invisible to the lesson/preference/skill auto-surfacing pipeline that fires on every future turn.
- Lessons (mistakes, corrections, "never do X again"): `session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...", severity="low|medium|high|critical", category="...")`
- Decisions: `session(action="capture", event_type="decision", title="...", content="...")`
- Notes/insights: `session(action="capture", event_type="note|insight", title="...", content="...")`
- Facts/preferences: `memory(action="create_node", node_type="fact|preference", title="...", content="...")`
- Documents: `memory(action="create_doc", title="...", content="...", doc_type="spec|general")`
- Todos: `memory(action="create_todo", title="...", todo_priority="high|medium|low")`
Do NOT use `create_memory`, `TodoWrite`, `todo_list`, or local file writes for persistence.

## Skills (IMPORTANT — Do Not Ignore Matched Skills)

When `context()` returns `[MATCHED_SKILLS]`, you **MUST run** the listed skills via `skill(action="run", name="...")`.
- Skills marked ⚡ (high-priority, priority ≥ 80) are **mandatory** — run them immediately before other work
- Skills marked ▶ (recommended, priority ≥ 60) should be run unless clearly irrelevant
- Skills marked ○ (available) are optional but often helpful

Reusable instruction + action bundles that persist across projects and sessions:
- Browse: `skill(action="list")` or `skill(action="list", scope="team")`
- Create: `skill(action="create", name="...", instruction_body="...", trigger_patterns=[...])`
- Update: `skill(action="update", name="...", instruction_body="...", change_summary="...")` (name or `skill_id`)
- Run: `skill(action="run", name="...")` — executes the skill's action pipeline
- Import: `skill(action="import", file_path="CLAUDE.md", format="auto")` — imports from any rules file
- Skills auto-activate when their trigger keywords match the user's message. The `context()` response surfaces them.

## Code Search

**ALWAYS** use ContextStream `search()` before Glob, Grep, Read, SemanticSearch, `code_search`, `grep_search`, or `find_by_name`.
Do NOT launch Task/explore subagents for code search — use `search(mode="auto", query="...")` directly.
ContextStream search results contain **real file paths, line numbers, and code content** — they ARE code results.
**NEVER** dismiss ContextStream results as "non-code" — use the returned file paths to `read_file` the relevant code.
Use `search(include_content=true)` to get inline code snippets in results.

## Context Pressure

When `context()` returns `context_pressure.level: "high"`:
- Save a session snapshot before compaction
- `session(action="capture", event_type="session_snapshot", title="...", content="...")`
- After compaction: `init(folder_path="...", is_post_compact=true)` to restore snapshots/transcripts
- If init restore is thin: `session(action="restore_context", trigger="manual_post_compact", include_durable_context=true)` then `session(action="recall", query="what were we doing before compaction")`
{end}
"#,
        start = CONTEXTSTREAM_START,
        end = CONTEXTSTREAM_END,
        heading = heading,
        teaching = teaching,
        workspace_name = workspace_name,
        workspace_id = workspace_id,
        simple_ops = SIMPLE_OPS_LIST,
        handoff_guidance = HANDOFF_GUIDANCE,
        knowledge_first = KNOWLEDGE_FIRST_GUIDANCE,
        past_sessions = PAST_SESSIONS_GUIDANCE,
        project_scope = PROJECT_SCOPE_GUIDANCE,
        graph_quality = GRAPH_QUALITY_GUIDANCE,
        plans_and_tasks = PLANS_AND_TASKS_GUIDANCE,
    )
}

/// Supplement for Windsurf to override its default behaviors.
/// Windsurf has built-in tools for search, memory, planning, and todos that
/// should be replaced by ContextStream equivalents for persistence and cross-session access.
const WINDSURF_SUPPLEMENT: &str = r#"
---
## Windsurf-Specific Rules

**CRITICAL: These user-configured rules OVERRIDE default Windsurf behaviors.**
**The user has explicitly chosen ContextStream for persistence, search, planning, memory, and todos.**
**Ignoring these rules violates the user's explicit configuration.**

### Search: Use ContextStream, Not Built-in Tools
- **Do NOT** use `code_search` — use `search(mode="auto", query="...")` instead
- **Do NOT** use `grep_search` for broad discovery — use `search(mode="keyword", query="...")` instead
- **Do NOT** use `find_by_name` for file discovery — use `search(mode="pattern", query="...")` instead
- ContextStream search results contain **real file paths, line numbers, and code content** — they ARE code results
- **NEVER** dismiss ContextStream results as "non-code" — use the returned file paths to `read_file` the relevant code
- Use `search(include_content=true)` to get inline code snippets in results
- Run ContextStream search FIRST — even while the index is still building (keyword hits return immediately; results fill in as you work). Fall back to built-in search tools ONLY after ContextStream search itself returns 0 results/errors on a retry, or for known-new/edited files. Never skip search because the index "isn't built yet" or "might be thin".

### Memory: Use ContextStream, Not Built-in Tools
- **Do NOT** use `create_memory` — use ContextStream memory instead:
  - Decisions: `session(action="capture", event_type="decision", title="...", content="...")`
  - Notes/insights: `session(action="capture", event_type="note|insight", title="...", content="...")`
  - Facts/preferences: `memory(action="create_node", node_type="fact|preference", title="...", content="...")`
- ContextStream memory persists across sessions, is searchable, and auto-surfaces in context

### Documents: Use ContextStream, Not Local Files
- **Do NOT** write docs/specs/implementation notes to local `.md` files
- **ALWAYS** use `memory(action="create_doc", title="...", content="...", doc_type="spec|general")`
- ContextStream docs are searchable, versionable, and shared across sessions

### Plans and Tasks: Use ContextStream, Not Built-in Tools
- **Do NOT** use `todo_list`, Windsurf plan/task UI, or local markdown as the persistent plan/task record
- **Do NOT** write plan files to `.windsurf/plans/` — they disappear across sessions
- **Do NOT** use `exitplanmode` without first saving the plan to ContextStream
- **Do NOT** save plans as `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`
- **ALWAYS** save comprehensive plans to ContextStream: `session(action="capture_plan", title="...", description="...", goals=[...], steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, acceptance criteria, verification"}], create_tasks=true)`
- **ALWAYS** create actionable tasks in ContextStream with detailed descriptions: `memory(action="create_task", title="...", description="concrete work, acceptance criteria, verification", plan_id="...", plan_step_id="...", priority="medium", task_status="pending")`
- **ALWAYS** list and update task state through ContextStream: `memory(action="list_tasks", plan_id="...")` and `memory(action="update_task", task_id="...", task_status="in_progress|completed|blocked")`
- **ALWAYS** link tasks to their plan with `plan_id` and, when available, `plan_step_id`

### Todos: Use ContextStream, Not Built-in Tools
- **Do NOT** use `todo_list` for persistent todos — use `memory(action="create_todo", title="...", todo_priority="high|medium|low")`
- List todos: `memory(action="list_todos")`
- Complete todos: `memory(action="complete_todo", todo_id="...")`
- ContextStream todos persist across sessions and are trackable"#;

/// Supplement for Cursor to override its default behaviors.
/// Cursor tends to use its built-in explore subagents, SemanticSearch, and
/// markdown files for plans instead of ContextStream equivalents.
const CURSOR_SUPPLEMENT: &str = r#"
---
## Cursor-Specific Rules

### Search: Use ContextStream, Not Built-in Tools
- **Do NOT** use `SemanticSearch` — use `search(mode="auto", query="...")` instead
- **Do NOT** launch `Task` subagents with `subagent_type="explore"` for code search
- **Do NOT** use `Grep` or `Glob` for broad code discovery
- ContextStream search results contain **real file paths, line numbers, and code content** — they ARE code results
- **NEVER** dismiss ContextStream results as "non-code" — use the returned file paths to `read_file` the relevant code
- Use `search(include_content=true)` to get inline code snippets in results
- Run ContextStream search FIRST — even while the index is still building (keyword hits return immediately; results fill in as you work). Fall back to local tools (Grep/Glob/Read) ONLY after ContextStream search itself returns 0 results/errors on a retry, or for known-new/edited files. Never skip search because the index "isn't built yet" or "might be thin".

### Memory: Use ContextStream, Not Local Files
- **Do NOT** write decisions/notes/implementation details to local files
- Use `session(action="capture", event_type="decision|insight|operation|uncategorized", title="...", content="...")`
- Use `memory(action="create_node", node_type="fact|preference", title="...", content="...")`
- ContextStream memory persists across sessions and auto-surfaces in context

### Documents: Use ContextStream, Not Local Files
- **Do NOT** write docs/specs/implementation notes to local `.md` files
- **ALWAYS** use `memory(action="create_doc", title="...", content="...", doc_type="spec|general")`

### Planning: Use ContextStream, Not Built-in Tools
- **Do NOT** create markdown plan files — plans disappear across sessions
- **Do NOT** use `SwitchMode` to plan mode without saving the plan to ContextStream afterward
- **Do NOT** use `TodoWrite` for plans — use `session(action="capture_plan")` and `memory(action="create_task")` instead
- **Do NOT** save plans as `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`
- **ALWAYS** save comprehensive plans: `session(action="capture_plan", title="...", description="...", goals=[...], steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, acceptance criteria, verification"}], create_tasks=true)`
- **ALWAYS** create linked tasks with details: `memory(action="create_task", title="...", description="concrete work, acceptance criteria, verification", plan_id="...", plan_step_id="...", priority="medium", task_status="pending")`

### Todos: Use ContextStream, Not Built-in Tools
- **Do NOT** use `TodoWrite` for persistent todos — use `memory(action="create_todo", title="...", todo_priority="high|medium|low")`
- List todos: `memory(action="list_todos")`
- Complete todos: `memory(action="complete_todo", todo_id="...")`
- ContextStream todos persist across sessions and are searchable"#;

/// Supplement for Codex/OpenCode to override their built-in search tools.
/// Codex uses Explore subagents and "Searched for" built-in operations that
/// bypass ContextStream's premium search entirely.
const CODEX_SUPPLEMENT: &str = r#"
---
## Codex/OpenCode-Specific Rules

**CRITICAL: ContextStream search() REPLACES all built-in search tools.**
**The user is paying for ContextStream's premium search — default tools must not bypass it.**

### Search: Use ContextStream, Not Built-in Tools
- **Do NOT** use `Explore` subagents for code discovery — use `search(mode="auto", query="...")` instead
- **Do NOT** use "Searched for files" or "Searched for <pattern>" built-in operations — use `search(mode="pattern", query="...")` instead
- **Do NOT** run shell commands for search (`grep`, `find`, `rg`, `fd`, `ack`) — use `search()` instead
- **Do NOT** scan directories or list files for discovery — use `search(mode="auto", query="...")` instead
- ContextStream search handles **all** search use cases: exact text, regex, glob patterns, semantic queries, file paths
- ContextStream search results contain **real file paths, line numbers, and code content** — they ARE code results
- **NEVER** dismiss ContextStream results as "non-code" — use the returned file paths to `read_file` the relevant code
- Run ContextStream search FIRST — even while the index is still building (keyword hits return immediately; results fill in as you work). Fall back to local/shell tools ONLY after ContextStream search itself returns 0 results/errors on a retry, or for known-new/edited files. Never skip search because the index "isn't built yet" or "might be thin".

### Search Mode Selection (use these instead of shell commands):
- Instead of `grep "pattern"`: use `search(mode="keyword", query="pattern")`
- Instead of `find . -name "*.tsx"`: use `search(mode="pattern", query="*.tsx")`
- Instead of `grep -E "regex"`: use `search(mode="pattern", query="regex")`
- Instead of exploring directories: use `search(mode="auto", query="<what you're looking for>")`

### Memory: Use ContextStream, Not Local Files
- **Do NOT** write decisions/notes/specs to local files
- Use `session(action="capture", event_type="decision|insight|operation|uncategorized", title="...", content="...")`
- Use `memory(action="create_doc", title="...", content="...", doc_type="spec|general")`

### Planning: Use ContextStream, Not Built-in Tools
- **Do NOT** create markdown plan files — they vanish across sessions
- **Do NOT** use Codex plan mode output (`plan_mode_respond`) as the persistent plan record — save the plan to ContextStream instead
- **Do NOT** use built-in todo/plan tools (`TodoWrite`, `todo_list`, `plan_mode_respond`) for persistent plans or tasks
- **Do NOT** save plans as `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`
- **ALWAYS** save comprehensive plans: `session(action="capture_plan", title="...", description="...", goals=[...], steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, acceptance criteria, verification"}], create_tasks=true)`
- **ALWAYS** create linked tasks with details: `memory(action="create_task", title="...", description="concrete work, acceptance criteria, verification", plan_id="...", plan_step_id="...", priority="medium", task_status="pending")`"#;

const COPILOT_SUPPLEMENT: &str = r#"
---
## VS Code Copilot Notes

- Keep this file concise; put detailed workflows in `.github/skills/contextstream-workflow/SKILL.md`
- Use ContextStream plans/tasks as the persistent record of work
- Save plans with `session(action="capture_plan", ..., create_tasks=true)`, not generic plan events; linked tasks need plan_id, plan_step_id, detailed descriptions, priority, and status
- Before code discovery, use `search(mode="auto", query="...")`
"#;

const ANTIGRAVITY_SUPPLEMENT: &str = r#"
---
## Antigravity-Specific Reliability Notes

- Antigravity currently has no documented lifecycle hooks for ContextStream enforcement.
- Treat ContextStream-first behavior as mandatory policy: run `context(...)` first, then `search(mode="auto", ...)` before local discovery.
- Keep `mcp_config.json` valid and minimal: preserve non-ContextStream servers and only update the `contextstream` block.
- If ContextStream appears skipped, verify:
  1. MCP server status is healthy in Antigravity settings
  2. Project is indexed and `search(mode="auto", ...)` is retried before local fallbacks
  3. Rule files contain the current ContextStream managed block
"#;

/// Supplement for Claude Code to override its built-in search tools.
/// Claude Code uses Grep, Glob, Task subagents, and parallel file scanning
/// that bypass ContextStream's premium search.
const CLAUDE_SUPPLEMENT: &str = r#"
---
## Claude Code-Specific Rules

**CRITICAL: ContextStream search() REPLACES all built-in search tools.**
**The user is paying for ContextStream's premium search — default tools must not bypass it.**

### Search: Use ContextStream, Not Built-in Tools
- **Do NOT** use `Grep` for code search — use `search(mode="keyword", query="...")` instead
- **Do NOT** use `Glob` for file discovery — use `search(mode="pattern", query="...")` instead
- **Do NOT** launch `Task` subagents with `subagent_type="explore"` — use `search(mode="auto", query="...")` instead
- **Do NOT** use parallel Grep/Glob calls for broad discovery — a single `search()` call replaces them all
- ContextStream search handles **all** search use cases: exact text, regex, glob patterns, semantic queries, file paths
- ContextStream search results contain **real file paths, line numbers, and code content** — they ARE code results
- **NEVER** dismiss ContextStream results as "non-code" — use the returned file paths to `read_file` the relevant code
- Run ContextStream search FIRST — even while the index is still building (keyword hits return immediately; results fill in as you work). Fall back to `Grep`/`Glob` ONLY after ContextStream search itself returns 0 results/errors on a retry, or for known-new/edited files. Never skip search because the index "isn't built yet" or "might be thin".

### Search Mode Selection (use these instead of built-in tools):
- Instead of `Grep("pattern")`: use `search(mode="keyword", query="pattern")`
- Instead of `Glob("**/*.tsx")`: use `search(mode="pattern", query="*.tsx")`
- Instead of `Grep` with regex: use `search(mode="pattern", query="regex")`
- Instead of `Task(subagent_type="explore")`: use `search(mode="auto", query="<what you're looking for>")`

### Memory: Use ContextStream, Not Local Files
- **Do NOT** write decisions/notes/specs to local files
- Use `session(action="capture", event_type="decision|insight|operation|uncategorized", title="...", content="...")`
- Use `memory(action="create_doc", title="...", content="...", doc_type="spec|general")`

### Planning: Use ContextStream, Not Built-in Tools
- **Do NOT** create markdown plan files or use `TodoWrite` — they vanish across sessions
- **Do NOT** save plans as `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`
- **ALWAYS** save comprehensive plans: `session(action="capture_plan", title="...", description="...", goals=[...], steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, acceptance criteria, verification"}], create_tasks=true)`
- **ALWAYS** create linked tasks with details: `memory(action="create_task", title="...", description="concrete work, acceptance criteria, verification", plan_id="...", plan_step_id="...", priority="medium", task_status="pending")`"#;

/// Supplement for editors without hooks.
/// These editors need explicit guidance since there's no automatic enforcement.
const NO_HOOKS_SUPPLEMENT: &str = r#"
---
## IMPORTANT: No Hooks Available

**This editor does NOT have hooks to enforce ContextStream behavior.**
You MUST follow these rules manually - there is no automatic enforcement.

## ContextStream Knowledge First

**Before guessing or struggling through an unfamiliar workflow, check ContextStream first.**
- Start with `context(...)` when that tool is exposed, or `session(action="ground", user_message="...")` when `context` is unavailable, and follow `[MATCHED_SKILLS]`, `[LESSONS_WARNING]`, `[PREFERENCE]`, and `<system-reminder>` output
- Treat `[LESSONS_WARNING]` as active working instructions for the current task, not optional background context
- If the task is unfamiliar, process-heavy, or likely documented already, inspect `skill(action="list")`, `memory(action="list_docs")`, `session(action="get_lessons")`, or `memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available before trial-and-error
- If `context()` or `session(action="ground", ...)` returns `[MATCHED_SKILLS]`, run the listed skills before other work

---

## SESSION START PROTOCOL

**On EVERY new session, you MUST:**

1. **Call `init(folder_path="<project_path>")`** FIRST
   - This triggers project indexing
   - Check response for `indexing_status`
   - If indexed coverage already exists, search immediately even while refresh continues; wait only when no usable index exists yet

2. **Generate a unique session_id** (e.g., `"session-" + timestamp` or a UUID)
   - Use this SAME session_id for ALL `context()` calls in this conversation

3. **Call `context(user_message="<first_message>", session_id="<id>")` if available; otherwise call `session(action="ground", user_message="<first_message>", session_id="<id>")`**
   - Gets task-specific rules, lessons, and preferences
   - Check for [LESSONS_WARNING], [PREFERENCE], [RULES_NOTICE]
   - If [LESSONS_WARNING] appears, treat those lessons as mandatory instructions for the task until it is finished

4. **Default behavior:** call `context(...)` first on each message when available; otherwise call `session(action="ground", user_message="...")`. Narrow bypass is allowed only for immediate read-only ContextStream calls when previous context is still fresh and no state-changing tool has run.

5. **Instruction alignment (if tool is exposed):** call `instruct(action="get", session_id="<id>", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` before `context(...)` each turn, and `instruct(action="ack", session_id="<id>", workspace_id="<current_workspace_id>", ids=[...])` after using entries. Reuse ids returned by init/context; if no project is resolved, omit project_id intentionally for workspace-only instructions rather than inferring it.

---

## TRANSCRIPT SAVING (OPTIONAL)

Transcripts are OFF by default.

### Enable for this chat:
```
context(user_message="<user's message>", save_exchange=true, session_id="<session-id>")
```

### Disable for this chat:
```
context(user_message="<user's message>", save_exchange=false, session_id="<session-id>")
```

### Default policy via MCP config env:
- `CONTEXTSTREAM_TRANSCRIPTS_ENABLED="true|false"`
- `CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED="true|false"`

### Session ID Guidelines:
- Generate ONCE at the start of the conversation
- Use a unique identifier (UUID or timestamp-based)
- Keep the SAME session_id for ALL context() calls
- Different sessions = different transcript preference state

---

## FILE INDEXING (CRITICAL)

**There is NO automatic file indexing in this editor.**
You MUST manage indexing manually:

**IMPORTANT: Hosted MCP is the preferred/default editor transport. The hosted gateway cannot read workstation disks directly, so `project(action="index")` sends a checkout-scoped refresh request to the installed managed sync bridge; editor hooks and ContextStream Desktop are additional ingest paths. A `requires_sync_bridge` response means the bridge is offline, unregistered, or lacks that checkout. Keep hosted MCP configured, repair bridge/hooks/Desktop, and retry. Local stdio MCP remains an explicit recovery option, not the recommended indexing path.**

### After Creating/Editing Files:
```
project(action="index")
```
If folder context is active, this resolves the current repo and uses the exact-checkout managed refresh path automatically.

### To Target A Specific Folder Or Recover From Stale Scope:
```
init(folder_path="<project_folder>")
project(action="index")
```

### Signs You Need to Re-index:
- Search doesn't find code you just wrote
- Search returns old versions of functions
- New files don't appear in search results

---

## SEARCH-FIRST (No PreToolUse Hook)

**There is NO hook to redirect local tools.** You MUST self-enforce:

### Before Broad Local Discovery, Check Index Status:
```
project(action="index_status")
```

### Search Protocol:
- **IF indexed (fresh/recent/aging/stale):** run `search(mode="...", query="...")` immediately before local tools. Do not wait for an instantly fresh index.
- **IF no usable index exists yet:** retry `search(mode="auto", ...)` after a short wait — keyword search returns committed results as the index builds; do not jump straight to local tools
- **IF search returns results with a stale-index advisory:** use those results for existing indexed code; refresh in background and retry only before concluding a newly edited/created symbol is absent
- **IF search returns 0 results after a targeted retry, or you are inspecting known-new local edits:** local tools are allowed

### Choose Search Mode Intelligently:
- `auto` (recommended): query-aware mode selection
- `hybrid`: mixed semantic + keyword retrieval for broad discovery
- `semantic`: conceptual questions ("how does X work?")
- `keyword`: exact text / quoted string
- `pattern`: glob or regex (`*.ts`, `foo\s+bar`)
- `refactor`: symbol usage / rename-safe lookup
- `exhaustive`: all occurrences / complete match coverage
- `team`: cross-project team search

### Output Format Hints:
- Use `output_format="paths"` for file listings and rename targets
- Use `output_format="count"` for "how many" queries

### Two-Phase Search Pattern (for precision):
- Pass 1 (discovery): `search(mode="auto", query="<concept + module>", output_format="paths", limit=10)`
- Pass 2 (precision): use one of:
  - exact text/symbol: `search(mode="keyword", query="\"exact_text\"", include_content=true)`
  - symbol usage: `search(mode="refactor", query="SymbolName", output_format="paths")`
  - all occurrences: `search(mode="exhaustive", query="symbol_or_text")`
- Then use local Read/Grep only on paths returned by ContextStream.

### When Local Tools Are OK (reactive only — never preemptive):
- ContextStream search itself returns 0 results or errors after a targeted retry
- You are inspecting known-new or recently edited files the index may not contain yet
- User explicitly requests local tools

**Always run ContextStream search FIRST, even while the index is still building.** "The index isn't ready / might be thin" is NOT a reason to skip it — keyword search returns committed results immediately and the index fills in as you work.

---

## CONTEXT COMPACTION (No PreCompact Hook)

**There is NO automatic state saving before compaction.**
You MUST save state manually when the conversation gets long:

### When to Save State:
- After completing a major task
- Before the conversation might be compacted
- If `context()` returns `context_pressure.level: "high"`

### How to Save State:
```
session(action="capture", event_type="session_snapshot",
  title="Session checkpoint",
  content="{ \"summary\": \"what we did\", \"active_files\": [...], \"next_steps\": [...] }")
```

### After Compaction (if context seems lost):
```
init(folder_path="...", is_post_compact=true)
session(action="restore_context", trigger="manual_post_compact", include_durable_context=true)
session(action="recall", query="what were we doing before compaction")
```

---

## PLANS & TASKS (CRITICAL)

**NEVER create markdown plan files** — they vanish across sessions and are not searchable.
**NEVER use built-in todo/plan tools** (e.g., `TodoWrite`, `todo_list`, `plan_mode_respond`) — use ContextStream instead.
**NEVER save plans as generic events** — do not use `session(action="capture", event_type="plan")` or `memory(action="create_event", event_type="plan")`.

**ALWAYS use ContextStream for planning:**

```
session(action="capture_plan",
  title="...",
  description="scope, constraints, affected areas, acceptance criteria, verification strategy",
  goals=["..."],
  steps=[{"id":"plan-step-1","title":"...","order":1,"description":"scope, concrete work, files/modules if known, acceptance criteria, verification"}],
  create_tasks=true)
memory(action="create_task",
  title="...",
  description="concrete work, acceptance criteria, verification",
  plan_id="<plan uuid>",
  plan_step_id="plan-step-1",
  priority="medium",
  task_status="pending")
```

Plans and tasks in ContextStream persist across sessions, are searchable, and auto-surface in context.

---

## MEMORY & DOCS (CRITICAL)

**NEVER use built-in memory tools** (e.g., `create_memory`) — use ContextStream instead.
**NEVER write docs/specs/notes to local files** — use ContextStream docs instead.

**ALWAYS use ContextStream for persistence:**

```
session(action="capture", event_type="decision|insight|operation|uncategorized", title="...", content="...")
memory(action="create_node", node_type="fact|preference", title="...", content="...")
memory(action="create_doc", title="...", content="...", doc_type="spec|general")
memory(action="create_todo", title="...", todo_priority="high|medium|low")
```

ContextStream memory, docs, and todos persist across sessions, are searchable, and auto-surface in context.

---

## VERSION UPDATES

**Check for updates periodically** using `help(action="version")`.

If the response includes [VERSION_NOTICE] or [VERSION_CRITICAL], tell the user about the available update.

### Update Commands:
```bash
# macOS/Linux
curl -fsSL https://contextstream.io/scripts/setup.sh | bash
# npm
npm install -g @contextstream/mcp-server@latest
```

---
"#;

/// Wrap content with ContextStream markers.
#[allow(dead_code)]
pub fn wrap_with_markers(content: &str) -> String {
    format!(
        "{}\n{}\n{}",
        CONTEXTSTREAM_START, content, CONTEXTSTREAM_END
    )
}

/// Find the first ContextStream block start marker in content.
fn find_block_start(content: &str) -> Option<(usize, &'static str)> {
    [CONTEXTSTREAM_START, LEGACY_START]
        .into_iter()
        .filter_map(|marker| content.find(marker).map(|pos| (pos, marker)))
        .min_by_key(|(pos, _)| *pos)
}

/// Find the first ContextStream block end marker after `search_from`.
fn find_block_end(content: &str, search_from: usize) -> Option<(usize, &'static str)> {
    [CONTEXTSTREAM_END, LEGACY_END]
        .into_iter()
        .filter_map(|marker| {
            content[search_from..]
                .find(marker)
                .map(|rel_pos| (search_from + rel_pos, marker))
        })
        .min_by_key(|(pos, _)| *pos)
}

/// Locate the ContextStream block bounds.
///
/// Supports mixed marker styles (e.g. `<contextstream>` + `<!-- END ContextStream -->`).
/// If a start marker exists but no end marker exists, the block extends to EOF.
fn find_contextstream_block_bounds(content: &str) -> Option<(usize, usize)> {
    let (start_pos, start_marker) = find_block_start(content)?;
    let search_from = start_pos + start_marker.len();

    // Prefer the matching end marker when present. Fall back to either marker
    // to support mixed start/end marker styles.
    let preferred_end = if start_marker == CONTEXTSTREAM_START {
        CONTEXTSTREAM_END
    } else {
        LEGACY_END
    };
    if let Some(rel_pos) = content[search_from..].find(preferred_end) {
        return Some((start_pos, search_from + rel_pos + preferred_end.len()));
    }

    if let Some((end_pos, end_marker)) = find_block_end(content, search_from) {
        Some((start_pos, end_pos + end_marker.len()))
    } else {
        Some((start_pos, content.len()))
    }
}

/// Replace or insert ContextStream block in content.
/// Recognizes both current `<contextstream>` and legacy `<!-- BEGIN ContextStream -->` markers.
pub fn replace_contextstream_block(existing: &str, new_block: &str) -> Result<String> {
    if existing.is_empty() {
        return Ok(new_block.to_string());
    }

    if let Some((start_pos, end_pos)) = find_contextstream_block_bounds(existing) {
        let block = &existing[start_pos..end_pos];
        if !block.contains(CONTEXTSTREAM_END) && !block.contains(LEGACY_END) {
            anyhow::bail!(
                "Refusing to modify rules with an opening ContextStream marker but no closing marker"
            );
        }
        if !contextstream_rules_block_is_owned(block) {
            anyhow::bail!(
                "Refusing to modify a <contextstream> rules block without a recognized ContextStream ownership marker"
            );
        }

        // Normal case: both markers present (including mixed marker styles).
        // Preserve every byte outside the owned block. In particular, trimming
        // here would silently rewrite user-authored spacing around the block.
        let before = &existing[..start_pos];
        let after = &existing[end_pos..];
        // The block bounds intentionally begin/end at the XML markers so
        // adjacent line endings remain user-owned exterior content. Generated
        // blocks may carry their own leading/trailing line ending; blindly
        // concatenating both grows blank lines on every refresh. Drop only the
        // generated block's duplicate boundary line endings. `before` and
        // `after` remain byte-for-byte untouched.
        let mut replacement = new_block;
        if before.ends_with('\r') || before.ends_with('\n') {
            replacement = replacement
                .strip_prefix("\r\n")
                .or_else(|| replacement.strip_prefix('\n'))
                .or_else(|| replacement.strip_prefix('\r'))
                .unwrap_or(replacement);
        }
        if after.starts_with('\r') || after.starts_with('\n') {
            replacement = replacement
                .strip_suffix("\r\n")
                .or_else(|| replacement.strip_suffix('\n'))
                .or_else(|| replacement.strip_suffix('\r'))
                .unwrap_or(replacement);
        }
        return Ok(format!("{}{}{}", before, replacement, after));
    }

    // Prepend to existing content
    Ok(format!("{}\n\n{}", new_block, existing))
}

/// Remove the ContextStream block from a rules file, preserving other content.
///
/// Returns the cleaned content, or None if the file didn't contain a block.
pub fn remove_contextstream_from_rules(
    editor: &Editor,
    project_path: Option<&Path>,
) -> Result<bool> {
    let mut removed = false;
    for path in editor.all_rules_cleanup_paths(project_path) {
        if !path.try_exists()? {
            continue;
        }
        removed |= remove_contextstream_from_path(&path)?;
    }

    if *editor == Editor::Copilot {
        if let Some(project_path) = project_path {
            let skill_path = project_path.join(COPILOT_SKILL_PATH);
            if skill_path.try_exists()? {
                let existing = std::fs::read_to_string(&skill_path)?;
                if copilot_skill_is_owned(&existing) {
                    let backup = safe_edit::backup_path(&skill_path)?;
                    let backup_content = safe_edit::read_recovery_file(&backup)?;
                    if backup_content
                        .as_deref()
                        .is_some_and(|content| !copilot_skill_is_owned(content))
                    {
                        anyhow::bail!(
                            "Refusing to remove {} because recovery backup {} is not a recognized ContextStream-owned skill",
                            skill_path.display(),
                            backup.display()
                        );
                    }
                    removed |= safe_edit::remove_owned_file_if_unchanged(&skill_path, &existing)?;
                    if let Some(backup_content) = backup_content {
                        safe_edit::remove_owned_file_if_unchanged(&backup, &backup_content)?;
                    }
                }
            }
        }
    }

    Ok(removed)
}

fn remove_contextstream_from_path(path: &Path) -> Result<bool> {
    let resolved_path = resolve_rules_file_path(path);
    if !resolved_path.try_exists()? {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&resolved_path)?;
    if content.is_empty() {
        return Ok(false);
    }
    if try_restore_exact_rules_backup(&resolved_path, &content)? {
        return Ok(true);
    }

    // Try marker-based removal (current, legacy, and mixed markers)
    if let Some((start_pos, end_pos)) = find_contextstream_block_bounds(&content) {
        let block = &content[start_pos..end_pos];
        if !block.contains(CONTEXTSTREAM_END) && !block.contains(LEGACY_END) {
            anyhow::bail!(
                "Refusing to remove rules from {}: an opening ContextStream marker has no closing marker",
                resolved_path.display()
            );
        }
        if !contextstream_rules_block_is_owned(block) {
            return Ok(false);
        }
        let generated_frontmatter = [WINDSURF_ALWAYS_ON_FRONTMATTER, CURSOR_MDC_FRONTMATTER]
            .into_iter()
            .find(|frontmatter| {
                content.starts_with(frontmatter)
                    && content[..start_pos].trim_end() == frontmatter.trim_end()
            });
        let backup_path = safe_edit::backup_path(&resolved_path)?;
        let backup = safe_edit::read_recovery_file(&backup_path)?;
        let generated_frontmatter_was_preexisting =
            generated_frontmatter.is_some_and(|frontmatter| {
                backup
                    .as_deref()
                    .is_some_and(|original| original.starts_with(frontmatter))
            });
        let adjusted_start =
            if generated_frontmatter.is_some() && !generated_frontmatter_was_preexisting {
                0
            } else {
                start_pos
            };
        let before = &content[..adjusted_start];
        let mut after = &content[end_pos..];
        if let Some(original) = backup.as_deref() {
            let original_body = if resolved_path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mdc"))
            {
                split_leading_frontmatter(original)
                    .map(|(_, body)| body)
                    .unwrap_or(original)
            } else {
                original
            };
            if !original_body.is_empty() {
                if let Some(offset) = after.find(original_body) {
                    if after[..offset]
                        .bytes()
                        .all(|byte| matches!(byte, b'\r' | b'\n'))
                    {
                        after = &after[offset..];
                    }
                }
            }
        } else if adjusted_start == 0 {
            after = after.trim_start_matches(['\r', '\n']);
        }
        let cleaned = format!("{}{}", before, after);
        if cleaned.trim().is_empty() {
            if backup.is_none() && adjusted_start == 0 {
                safe_edit::remove_owned_file_if_unchanged(&resolved_path, &content)?;
            } else {
                safe_edit::remove_file_if_unchanged(&resolved_path, &content)?;
            }
        } else {
            safe_edit::write_if_unchanged(&resolved_path, &cleaned, Some(&content))?;
        }
        return Ok(true);
    }

    Ok(false)
}

fn try_restore_exact_rules_backup(path: &Path, current: &str) -> Result<bool> {
    let Some((start, end)) = find_contextstream_block_bounds(current) else {
        return Ok(false);
    };
    if end == current.len()
        && !current[start..].contains(CONTEXTSTREAM_END)
        && !current[start..].contains(LEGACY_END)
    {
        return Ok(false);
    }
    if !contextstream_rules_block_is_owned(&current[start..end]) {
        return Ok(false);
    }

    let backup_path = safe_edit::backup_path(path)?;
    let backup = match safe_edit::read_recovery_file(&backup_path)? {
        Some(backup) => backup,
        None => return Ok(false),
    };
    if find_contextstream_block_bounds(&backup).is_some() {
        // Restoring this snapshot would leave ContextStream installed.
        return Ok(false);
    }

    let whitespace_end = current[end..]
        .char_indices()
        .find_map(|(offset, character)| (!character.is_whitespace()).then_some(end + offset))
        .unwrap_or(current.len());
    let mut candidate_ends = vec![end];
    candidate_ends.extend(
        current[end..whitespace_end]
            .char_indices()
            .skip(1)
            .map(|(offset, _)| end + offset),
    );
    candidate_ends.push(whitespace_end);
    candidate_ends.sort_unstable();
    candidate_ends.dedup();

    let exact_match = candidate_ends.into_iter().any(|candidate_end| {
        let managed_block = &current[start..candidate_end];
        render_contextstream_block_for_path(path, &backup, managed_block)
            .is_ok_and(|rendered| rendered == current)
    });
    if !exact_match {
        return Ok(false);
    }

    safe_edit::restore_text_first_backup(path, current, true, &backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_test_mutex;
    use tempfile::tempdir;

    #[test]
    fn test_generate_bootstrap_rules_claude() {
        let rules = generate_rule_content(
            &Editor::ClaudeCode,
            Some("test-id"),
            Some("Test"),
            RulesMode::Bootstrap,
        );
        assert!(rules.contains(CONTEXTSTREAM_START));
        assert!(rules.contains(CONTEXTSTREAM_END));
        // Claude Code should have prefixed tool names
        assert!(rules.contains("mcp__contextstream__context"));
        assert!(rules.contains("mcp__contextstream__init"));
        assert!(rules.contains("mcp__contextstream__search"));
        assert!(rules.contains("# ContextStream Rules"));
        assert!(rules.contains("## Core ContextStream Workflow"));
        assert!(rules.contains("## Detailed Rules"));
        assert!(rules.contains(mcp_types::HARNESS_TEACHING_VERSION));
        assert!(rules.contains("session(action=\"ground\", user_message=\"...\")"));
        assert!(rules.contains("Code Health and Dependency Recommendations"));
        assert!(rules.contains("quality_trends"));
        assert!(rules.contains("Do NOT save plans this way"));
        assert!(rules.contains("event_type=\"plan\""));
        assert!(rules.contains("create_tasks=true"));
        assert!(rules.contains("plan_step_id"));
    }

    #[test]
    fn test_generate_bootstrap_rules_cursor() {
        let rules = generate_rule_content(
            &Editor::Cursor,
            Some("test-id"),
            Some("Test"),
            RulesMode::Bootstrap,
        );
        // Cursor should NOT have prefixed tool names
        assert!(rules.contains("`context("));
        assert!(rules.contains("`init("));
        assert!(rules.contains("`search("));
        assert!(!rules.contains("mcp__contextstream__"));
        assert!(rules.contains("# ContextStream Rules"));
        assert!(rules.contains(mcp_types::HARNESS_TEACHING_VERSION));
    }

    #[test]
    fn test_generate_bootstrap_rules_codex() {
        let rules = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Bootstrap,
        );
        // Codex should NOT have prefixed tool names
        assert!(!rules.contains("mcp__contextstream__"));
        assert!(rules.contains("# ContextStream Rules"));
        assert!(rules.contains(mcp_types::HARNESS_TEACHING_VERSION));
        assert!(rules.contains("if `context` is unavailable"));
        assert!(rules.contains("`capsule(action=\"create\""));
        assert!(rules.contains("NEVER substitute a handoff entity"));
        // Codex has no hooks, so even bootstrap should get NO_HOOKS_SUPPLEMENT
        // (but only through write_project_rules which uses Full mode)
    }

    #[test]
    fn codex_and_claude_rules_require_workspace_id_on_every_task_operation() {
        for editor in [Editor::Codex, Editor::ClaudeCode] {
            let rules =
                generate_rule_content(&editor, Some("test-id"), Some("Test"), RulesMode::Full);
            assert!(
                rules.contains("Every task operation must include `workspace_id`")
                    && rules.contains("memory_update_task")
                    && rules.contains("memory(action=\"list_tasks\", workspace_id=")
                    && rules.contains("A `task_id` does not replace workspace scope"),
                "missing task workspace contract for {editor:?}"
            );
        }
    }

    #[test]
    fn every_editor_and_rules_mode_embeds_one_shared_teaching_contract() {
        for editor in Editor::all() {
            for mode in [RulesMode::Bootstrap, RulesMode::Minimal, RulesMode::Full] {
                let rules = generate_rule_content(editor, Some("test-id"), Some("Test"), mode);
                assert!(
                    rules.contains(mcp_types::HARNESS_TEACHING_VERSION),
                    "missing teaching version for {editor:?} {mode:?}"
                );
                assert_eq!(
                    rules.matches("contextstream-teaching-version:").count(),
                    1,
                    "duplicated teaching contract for {editor:?} {mode:?}"
                );
                assert!(
                    rules.contains("Initialize once")
                        && rules.contains("Ground every turn")
                        && rules.contains("Search before local discovery")
                        && rules.contains("Consult durable knowledge")
                        && rules.contains("Persist durable work canonically"),
                    "incomplete teaching contract for {editor:?} {mode:?}"
                );
                assert!(
                    rules.contains("workspace_id is mandatory")
                        && rules.contains("workspace_id=\"<current_workspace_id>\""),
                    "missing mandatory workspace scope teaching for {editor:?} {mode:?}"
                );
                assert!(
                    !rules.contains("before or after context"),
                    "contradictory initialization order for {editor:?} {mode:?}"
                );
                assert!(
                    rules.contains("do not immediately call")
                        && rules.contains("the first explicit escalation")
                        && rules.contains("absent, thin, stale, off-topic"),
                    "missing duplicate-recall guardrail for {editor:?} {mode:?}"
                );
                assert!(
                    !rules.contains("always the first call"),
                    "unconditional continuation recall remains for {editor:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn test_capsule_requests_require_the_capsule_tool_and_link_result() {
        let codex = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        let claude = generate_rule_content(
            &Editor::ClaudeCode,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );

        assert!(codex.contains("`capsule(action=\"create\""));
        assert!(claude.contains("`mcp__contextstream__capsule(action=\"create\""));
        for rules in [&codex, &claude] {
            assert!(rules.contains("return the capsule id plus Agent URL and Dashboard URL"));
            assert!(rules.contains("NEVER substitute a handoff entity"));
        }
    }

    #[test]
    fn test_no_hooks_supplement_for_codex() {
        let rules = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("No Hooks Available"));
        assert!(rules.contains("SESSION START PROTOCOL"));
        assert!(rules.contains("TRANSCRIPT SAVING"));
        assert!(rules.contains("FILE INDEXING"));
        assert!(rules.contains("SEARCH-FIRST"));
        assert!(rules.contains("CONTEXT COMPACTION"));
        assert!(rules.contains("restore_context"));
        assert!(rules.contains("manual_post_compact"));
        assert!(rules.contains("ContextStream Knowledge First"));
        assert!(rules.contains("plan_mode_respond"));
        assert!(rules.contains("memory(action=\"create_task\""));
        assert!(rules.contains("NEVER save plans as generic events"));
        assert!(rules.contains("create_tasks=true"));
        assert!(rules.contains("plan_step_id"));
        assert!(rules.contains("session(action=\"ground\", user_message=\"...\")"));
    }

    #[test]
    fn test_rules_include_project_routing_notice_guidance() {
        let claude_rules = generate_rule_content(
            &Editor::ClaudeCode,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        let codex_rules = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );

        for rules in [claude_rules, codex_rules] {
            assert!(rules.contains("[PROJECT_ROUTING]"));
            assert!(rules.contains("needs_project_selection"));
            assert!(rules.contains("needs_project_setup"));
            assert!(rules.contains("rerun `init/context` with `folder_path`"));
        }
    }

    #[test]
    fn test_no_hooks_supplement_for_opencode() {
        let rules = generate_rule_content(
            &Editor::OpenCode,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("No Hooks Available"));
        assert!(rules.contains("SESSION START PROTOCOL"));
        assert!(rules.contains("TRANSCRIPT SAVING"));
        assert!(rules.contains("FILE INDEXING"));
        assert!(rules.contains("SEARCH-FIRST"));
        assert!(rules.contains("CONTEXT COMPACTION"));
    }

    #[test]
    fn test_no_hooks_supplement_not_for_claude() {
        let rules = generate_rule_content(
            &Editor::ClaudeCode,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(!rules.contains("No Hooks Available"));
        assert!(!rules.contains("SESSION START PROTOCOL"));
    }

    #[test]
    fn test_no_hooks_supplement_for_aider() {
        let rules = generate_rule_content(
            &Editor::Aider,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("No Hooks Available"));
        assert!(rules.contains("FILE INDEXING"));
    }

    #[test]
    fn test_no_hooks_supplement_for_antigravity() {
        let rules = generate_rule_content(
            &Editor::Antigravity,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("No Hooks Available"));
        assert!(rules.contains("Antigravity-Specific Reliability Notes"));
    }

    #[test]
    fn test_no_hooks_supplement_for_copilot() {
        let rules = generate_rule_content(
            &Editor::Copilot,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("No Hooks Available"));
        assert!(rules.contains("SESSION START PROTOCOL"));
        assert!(rules.contains("VS Code Copilot Notes"));
    }

    #[test]
    fn test_windsurf_rules_require_contextstream_plans_and_tasks() {
        let rules = generate_rule_content(
            &Editor::Windsurf,
            Some("test-id"),
            Some("Test"),
            RulesMode::Bootstrap,
        );

        assert!(rules.starts_with(WINDSURF_ALWAYS_ON_FRONTMATTER));
        assert!(rules.contains("Windsurf-Specific Rules"));
        assert!(rules.contains("Plans and Tasks: Use ContextStream"));
        assert!(rules.contains("session(action=\"capture_plan\""));
        assert!(rules.contains("memory(action=\"create_task\""));
        assert!(rules.contains("memory(action=\"list_tasks\""));
        assert!(rules.contains("memory(action=\"update_task\""));
        assert!(rules.contains("plan_step_id"));
        assert!(rules.contains("Windsurf plan/task UI"));
    }

    #[test]
    fn test_no_hooks_supplement_not_for_cline() {
        let rules = generate_rule_content(
            &Editor::Cline,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(!rules.contains("No Hooks Available"));
    }

    #[test]
    fn test_no_hooks_rules_default_search_mode_to_auto() {
        let rules = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("search(mode=\"auto\", query=\"...\")"));
        assert!(!rules.contains("search(mode=\"...\", query=\"...\")"));
        assert!(rules.contains("mode=\"auto\""));
        assert!(!rules.contains("mode=\"...\""));
    }

    #[test]
    fn every_rules_mode_scopes_instruction_reads_explicitly() {
        for editor in Editor::all() {
            for mode in [RulesMode::Bootstrap, RulesMode::Minimal, RulesMode::Full] {
                let rules = generate_rule_content(editor, None, None, mode);
                assert!(
                    rules.contains(
                        "workspace_id=\"<current_workspace_id>\", project_id=\"<current_project_id>\""
                    ),
                    "missing explicit instruct scope for {editor:?} {mode:?}"
                );
                assert!(
                    rules.contains("omit project_id intentionally for workspace-only"),
                    "missing workspace-only fail-closed guidance for {editor:?} {mode:?}"
                );
                assert!(
                    !rules.contains("instruct(action=\"get\", session_id=\"...\")"),
                    "found unsafe unscoped instruct guidance for {editor:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn test_rules_surface_diagram_types() {
        let rules = generate_rule_content(
            &Editor::Codex,
            Some("test-id"),
            Some("Test"),
            RulesMode::Full,
        );
        assert!(rules.contains("memory(action=\"create_diagram\""));
        assert!(
            rules.contains("diagram_type=\"flowchart|sequence|class|er|gantt|mindmap|pie|other\"")
        );
        assert!(rules.contains("memory(action=\"list_diagrams\")"));
    }

    #[test]
    fn test_mcp_prefix_only_for_claude() {
        let claude = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Minimal);
        let cursor = generate_rule_content(&Editor::Cursor, None, None, RulesMode::Minimal);

        assert!(claude.contains("mcp__contextstream__context"));
        assert!(!cursor.contains("mcp__contextstream__"));
    }

    #[test]
    fn test_editor_headings() {
        let claude = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Bootstrap);
        assert!(claude.contains("# ContextStream Rules"));

        let cursor = generate_rule_content(&Editor::Cursor, None, None, RulesMode::Bootstrap);
        assert!(cursor.contains("# ContextStream Rules"));

        let cline = generate_rule_content(&Editor::Cline, None, None, RulesMode::Bootstrap);
        assert!(cline.contains("# ContextStream Rules"));
    }

    #[test]
    fn test_replace_contextstream_block_empty() {
        let result = replace_contextstream_block("", "new content").expect("replace empty rules");
        assert_eq!(result, "new content");
    }

    #[test]
    fn user_contextstream_heading_is_not_treated_as_owned_content() {
        let existing =
            "# Team notes\n\n# ContextStream API troubleshooting\nNever delete this runbook.\n";
        let result =
            replace_contextstream_block(existing, "<contextstream>\nmanaged\n</contextstream>")
                .expect("prepend managed block");

        assert!(result.contains("# ContextStream API troubleshooting"));
        assert!(result.contains("Never delete this runbook."));
        assert!(result.starts_with("<contextstream>"));
    }

    #[test]
    fn test_replace_contextstream_block_existing() {
        let existing = format!(
            "Before\n{}\n{} 0123456789abcdef -->\nOld content\n{}\nAfter",
            CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
        );
        let result =
            replace_contextstream_block(&existing, "new block").expect("replace managed block");
        assert!(result.contains("Before"));
        assert!(result.contains("new block"));
        assert!(result.contains("After"));
        assert!(!result.contains("Old content"));
    }

    #[test]
    fn replacement_preserves_every_byte_outside_owned_block() {
        let before = "User prefix  \n\t\n";
        let after = "\r\n \tUser suffix\n";
        let existing = format!(
            "{before}{CONTEXTSTREAM_START}\n{RULES_HASH_MARKER_PREFIX} 0123456789abcdef -->\nold\n{CONTEXTSTREAM_END}{after}"
        );

        let result =
            replace_contextstream_block(&existing, "replacement").expect("replace managed block");

        assert_eq!(result, format!("{before}replacement{after}"));
    }

    #[test]
    fn unmarked_user_contextstream_block_fails_closed_and_survives_uninstall() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("AGENTS.md");
        let existing = concat!(
            "# User instructions\n\n",
            "<contextstream>\n",
            "This is my own domain-specific XML section.\n",
            "</contextstream>\n\n",
            "# Keep this too\n",
        );
        std::fs::write(&path, existing).expect("seed user rules");
        let backup = safe_edit::backup_path(&path).expect("backup path");
        std::fs::write(&backup, "# unrelated stale recovery file\n").expect("seed stale backup");

        let install_error = write_contextstream_block_to_path(
            &path,
            "<contextstream>\nmanaged\n</contextstream>",
            true,
        )
        .expect_err("ambiguous user block must not be replaced");
        assert!(install_error
            .to_string()
            .contains("without a recognized ContextStream ownership marker"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);

        assert!(
            !remove_contextstream_from_path(&path).expect("unowned block must be ignored"),
            "uninstall must report that it removed nothing"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "# unrelated stale recovery file\n"
        );
    }

    #[test]
    fn recognizable_unmarked_legacy_xml_template_remains_migratable() {
        let old = concat!(
            "<contextstream>\n",
            "# Workspace: Engineering\n",
            "# Workspace ID: 11111111-2222-4333-8444-555555555555\n",
            "# ContextStream Rules\n",
            "**MANDATORY STARTUP:** call init.\n",
            "</contextstream>\n",
        );

        assert!(content_has_owned_contextstream_rules(old));
        assert_eq!(
            replace_contextstream_block(old, "replacement").expect("migrate legacy rules"),
            "replacement\n"
        );
    }

    #[test]
    fn test_replace_contextstream_block_mixed_markers_new_start_legacy_end() {
        let existing = format!(
            "Before\n{}\nOld content\n{}\nAfter",
            CONTEXTSTREAM_START, LEGACY_END
        );
        let result = replace_contextstream_block(&existing, "new block")
            .expect("replace mixed managed block");
        assert!(result.contains("Before"));
        assert!(result.contains("new block"));
        assert!(result.contains("After"));
        assert!(!result.contains("Old content"));
    }

    #[test]
    fn test_replace_contextstream_block_mixed_markers_legacy_start_new_end() {
        let existing = format!(
            "Before\n{}\nOld content\n{}\nAfter",
            LEGACY_START, CONTEXTSTREAM_END
        );
        let result = replace_contextstream_block(&existing, "new block")
            .expect("replace mixed managed block");
        assert!(result.contains("Before"));
        assert!(result.contains("new block"));
        assert!(result.contains("After"));
        assert!(!result.contains("Old content"));
    }

    #[test]
    fn test_replace_contextstream_block_nested_legacy_and_new_markers() {
        let existing = format!(
            "Before\n{}\n{}\nOld content\n{}\n{}\nAfter",
            LEGACY_START, CONTEXTSTREAM_START, CONTEXTSTREAM_END, LEGACY_END
        );
        let result = replace_contextstream_block(&existing, "new block")
            .expect("replace nested legacy managed block");
        assert!(result.contains("Before"));
        assert!(result.contains("new block"));
        assert!(result.contains("After"));
        assert!(!result.contains("Old content"));
        assert!(!result.contains(LEGACY_END));
    }

    #[test]
    fn test_wrap_with_markers() {
        let wrapped = wrap_with_markers("content");
        assert!(wrapped.starts_with(CONTEXTSTREAM_START));
        assert!(wrapped.ends_with(CONTEXTSTREAM_END));
    }

    #[test]
    fn test_full_mode_includes_search_protocol() {
        let rules = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Full);
        assert!(rules.contains("Search Protocol"));
        assert!(rules.contains("Two-Phase Search Playbook"));
        assert!(rules.contains("search(mode=\"auto\", query=\"<concept + module>\""));
        assert!(rules.contains("Context Pressure"));
        assert!(rules.contains("capture_plan"));
    }

    #[test]
    fn test_full_mode_uses_stable_update_script() {
        let rules = generate_rule_content(&Editor::Codex, None, None, RulesMode::Full);
        assert!(rules.contains("https://contextstream.io/scripts/setup.sh"));
        assert!(!rules.contains("setup-beta.sh"));
    }

    #[test]
    fn test_rules_define_an_exact_fast_direct_read_lane() {
        for editor in Editor::all() {
            for mode in [RulesMode::Bootstrap, RulesMode::Minimal, RulesMode::Full] {
                let rules = generate_rule_content(editor, None, None, mode);
                assert!(
                    rules.contains("Fast direct-read lane (no redundant grounding call)"),
                    "missing direct-read lane for {editor:?} {mode:?}"
                );
                assert!(
                    rules.contains("without another"),
                    "missing direct-call instruction for {editor:?} {mode:?}"
                );
                assert!(
                    !rules.contains("Narrow bypass only"),
                    "legacy ambiguous bypass survived for {editor:?} {mode:?}"
                );
            }
        }

        let rules = generate_rule_content(&Editor::Codex, None, None, RulesMode::Full);
        let lane_start = rules
            .find("**Fast direct-read lane")
            .expect("direct-read lane start");
        let lane_end = rules[lane_start..]
            .find("**Common queries")
            .map(|offset| lane_start + offset)
            .expect("direct-read lane end");
        let lane = &rules[lane_start..lane_end];

        for allowed in [
            "`workspace(action=\"list\"|\"get\")`",
            "`memory(action=\"list_docs\"|\"list_events\"|\"list_todos\"|\"list_tasks\"|\"list_transcripts\"|\"list_nodes\"|\"list_diagrams\", workspace_id=\"<current_workspace_id>\", project_id=\"<current_project_id>\")`",
            "`help(action=\"version\"|\"tools\"|\"auth\")`",
            "`project(action=\"list\"|\"get\"|\"index_status\")`",
            "`reminder(action=\"list\"|\"active\")`",
        ] {
            assert!(lane.contains(allowed), "missing exact direct read {allowed}");
        }

        for forbidden in [
            "workspace(action=\"create\"",
            "memory(action=\"decisions\"",
            "memory(action=\"get_doc\"",
            "session(action=\"recall\"",
            "media(action=\"search\"",
        ] {
            assert!(
                !lane.contains(forbidden),
                "grounding-sensitive operation leaked into direct lane: {forbidden}"
            );
        }
    }

    #[test]
    fn test_rules_emphasize_knowledge_first_before_improvising() {
        for (editor, mode) in [
            (&Editor::ClaudeCode, RulesMode::Bootstrap),
            (&Editor::KiloCode, RulesMode::Minimal),
            (&Editor::Codex, RulesMode::Full),
        ] {
            let rules = generate_rule_content(editor, None, None, mode);
            // Heading for the knowledge-first section.
            assert!(rules.contains("Finding Information — Search ContextStream Knowledge"));
            // Decision-table entries for every major knowledge surface.
            assert!(rules.contains("memory(action=\"decisions\""));
            assert!(rules.contains("memory(action=\"list_docs\")"));
            assert!(rules.contains("memory(action=\"get_doc\""));
            assert!(rules.contains("session(action=\"get_lessons\""));
            assert!(rules.contains("memory(action=\"list_nodes\""));
            assert!(rules.contains("memory(action=\"list_tasks\")"));
            assert!(rules.contains("memory(action=\"list_todos\")"));
            assert!(rules.contains("session(action=\"list_plans\")"));
            assert!(rules.contains("skill(action=\"list\")"));
            assert!(rules.contains("session(action=\"recall\""));
            assert!(rules.contains("memory(action=\"search\""));
            assert!(rules.contains("[MATCHED_SKILLS]"));
            assert!(rules.contains("[GROUNDING]"));
            assert!(rules.contains("[GROUNDING_AVAILABLE]"));
            assert!(rules.contains("active working instructions for the current task"));
            assert!(rules.contains("Project Scope Discipline"));
            assert!(rules.contains("Reuse the `project_id` returned by"));
        }
    }

    #[test]
    fn every_rules_mode_requires_current_source_for_production_diagnosis() {
        for editor in Editor::all() {
            for mode in [RulesMode::Bootstrap, RulesMode::Minimal, RulesMode::Full] {
                let rules = generate_rule_content(editor, None, None, mode);
                for required in [
                    "Checkout Currency Before Production Diagnosis",
                    "fetch the upstream/deployed ref",
                    "every inspected path is proven identical",
                    "Never pull, reset, rebase, or overwrite a dirty checkout",
                    "separate clean checkout",
                ] {
                    assert!(
                        rules.contains(required),
                        "{} {:?} rules must prevent stale-checkout production diagnosis: {required}",
                        editor.display_name(),
                        mode
                    );
                }
            }
        }
    }

    #[test]
    fn test_stamp_block_inserts_canonical_hash_marker_after_opening_tag() {
        // The marker has to land inside the `<contextstream>` block and
        // before any rule content, so a future read can grab it without
        // needing to parse the whole file. Place it on its own line for
        // readability.
        let block = "<contextstream>\n# Workspace: Engineering\n</contextstream>\n";
        let stamped = stamp_block_with_canonical_rules_hash(block);
        let marker_pos = stamped
            .find("<!-- contextstream-rules-hash:")
            .expect("marker must be present after stamping");
        let opening_pos = stamped
            .find(CONTEXTSTREAM_START)
            .expect("opening tag must remain present");
        assert!(
            marker_pos > opening_pos,
            "marker must follow `<contextstream>` opening tag"
        );
        // Round-trip: stripping and re-stamping yields the same hash.
        // Without this the hash would change on every write and the
        // staleness check would false-fire on every `generate_rules()`.
        let restamped = stamp_block_with_canonical_rules_hash(&stamped);
        let h1 = mcp_types::rules_hash::extract_hash_marker(&stamped).unwrap();
        let h2 = mcp_types::rules_hash::extract_hash_marker(&restamped).unwrap();
        assert_eq!(h1, h2, "stamping must be idempotent on identical content");
        assert_eq!(h1, canonical_rules_bundle_hash());
    }

    #[test]
    fn canonical_rules_bundle_hash_is_stable_and_content_sensitive() {
        let canonical = compute_canonical_rules_bundle_hash();
        assert_eq!(canonical.len(), 16);
        assert_ne!(canonical, "0000000000000000");
        assert_eq!(canonical, compute_canonical_rules_bundle_hash());

        assert_ne!(
            canonical,
            compute_rules_bundle_hash("deliberately-different-teaching-contract"),
            "changing bundled teaching material must change the fingerprint"
        );
    }

    #[test]
    fn every_editor_surface_uses_the_same_canonical_bundle_marker() {
        let expected = canonical_rules_bundle_hash();
        for editor in Editor::all() {
            let global = global_rules_content(
                editor,
                Some("11111111-1111-1111-1111-111111111111"),
                Some("Custom Workspace"),
            );
            let project = project_rules_content(
                editor,
                Some("22222222-2222-2222-2222-222222222222"),
                Some("Another Workspace"),
                Some("custom-project"),
            );
            for (surface, content) in [("global", global), ("project", project)] {
                let stamped = stamp_block_with_canonical_rules_hash(&content);
                assert_eq!(
                    mcp_types::rules_hash::extract_hash_marker(&stamped).as_deref(),
                    Some(expected),
                    "{} {} rules must use the process-wide teaching-bundle fingerprint",
                    editor.display_name(),
                    surface
                );
            }
        }
    }

    #[test]
    fn aider_hash_marker_remains_a_yaml_comment() {
        for content in [
            global_rules_content(&Editor::Aider, None, None),
            project_rules_content(&Editor::Aider, None, None, None),
        ] {
            let stamped = stamp_block_with_canonical_rules_hash(&content);
            let restamped = stamp_block_with_canonical_rules_hash(&stamped);
            assert_eq!(
                restamped, stamped,
                "re-stamping Aider YAML must be byte-identical"
            );
            let marker_line = stamped
                .lines()
                .find(|line| line.contains(RULES_HASH_MARKER_PREFIX))
                .expect("Aider rules must carry a bundle marker");
            assert!(
                marker_line.starts_with("# "),
                "Aider's YAML config must not gain an unknown top-level key: {marker_line}"
            );
            assert_eq!(
                mcp_types::rules_hash::extract_hash_marker(&stamped).as_deref(),
                Some(canonical_rules_bundle_hash())
            );
        }
    }

    #[test]
    fn test_rules_warn_against_redundant_clarifying_questions() {
        // Regression guard for the gap reported when an agent read a runbook
        // saying prod was on Crunchy Bridge, then *still* asked the user
        // "where is prod running?". The rules must explicitly forbid this
        // anti-pattern so it doesn't recur after future tightening of the
        // knowledge-first content.
        for (editor, mode) in [
            (&Editor::ClaudeCode, RulesMode::Bootstrap),
            (&Editor::KiloCode, RulesMode::Minimal),
            (&Editor::Codex, RulesMode::Full),
        ] {
            let rules = generate_rule_content(editor, None, None, mode);
            assert!(
                rules.contains("Don't re-ask what you just read"),
                "{} {:?} rules must call out the don't-re-ask anti-pattern",
                editor.display_name(),
                mode
            );
            assert!(
                rules.contains("Clarifying-question budget")
                    || rules.contains("clarifying-question budget"),
                "{} {:?} rules must establish a budget for clarifying questions",
                editor.display_name(),
                mode
            );
            assert!(
                rules.contains("clarifying questions are a last resort"),
                "{} {:?} rules must say clarifying questions are a last resort",
                editor.display_name(),
                mode
            );
        }
    }

    #[test]
    fn test_rules_surface_media_asset_tooling() {
        for (editor, mode) in [
            (&Editor::ClaudeCode, RulesMode::Bootstrap),
            (&Editor::Cursor, RulesMode::Minimal),
            (&Editor::Codex, RulesMode::Full),
        ] {
            let rules = generate_rule_content(editor, None, None, mode);
            assert!(
                rules.contains("Media assets (photos/images, video, audio, documents/PDFs)"),
                "{} {:?} should describe media assets as a first-class knowledge surface",
                editor.display_name(),
                mode
            );
            assert!(rules.contains("media(action=\"list\""));
            assert!(rules.contains("media(action=\"search\""));
            assert!(rules.contains("media(action=\"index\""));
            assert!(rules.contains("docs/PDFs/slides map to `document`"));
        }
    }

    #[test]
    fn every_rules_mode_makes_contextstream_handoffs_canonical() {
        for editor in Editor::all() {
            for mode in [RulesMode::Bootstrap, RulesMode::Minimal, RulesMode::Full] {
                let rules = generate_rule_content(editor, None, None, mode);
                for required in [
                    "Canonical Agent Handoffs",
                    "prepare/create a handoff",
                    "verified facts",
                    "eliminated hypotheses",
                    "HANDOFF.md",
                    "not** the canonical handoff",
                    "to_user_id",
                    "never invent a recipient",
                    "purpose=\"handoff\"",
                ] {
                    assert!(
                        rules.contains(required),
                        "{} {:?} rules must contain canonical handoff guidance: {required}",
                        editor.display_name(),
                        mode
                    );
                }
            }
        }
    }

    #[test]
    fn claude_rules_use_the_exposed_prefixed_entity_tool() {
        let rules = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Bootstrap);
        assert!(rules.contains("mcp__contextstream__entity(kind=\"handoff\""));
        assert!(rules.contains("mcp__contextstream__capsule(action=\"create\""));
        assert!(
            !rules.contains("`entity(kind=\"handoff\""),
            "Claude must never be taught an unexposed bare entity tool name"
        );
    }

    #[test]
    fn copilot_skill_carries_the_same_canonical_handoff_policy() {
        let skill = build_copilot_skill_content();
        assert!(skill.contains(HANDOFF_GUIDANCE.trim()));
        assert!(skill.contains("entity(kind=\"handoff\""));
        assert!(skill.contains("HANDOFF.md"));
    }

    #[test]
    fn test_indexing_transport_guidance_in_all_modes() {
        for (editor, mode) in [
            (&Editor::ClaudeCode, RulesMode::Bootstrap),
            (&Editor::Cursor, RulesMode::Bootstrap),
            (&Editor::Codex, RulesMode::Full),
            (&Editor::OpenCode, RulesMode::Full),
        ] {
            let rules = generate_rule_content(editor, None, None, mode);
            assert!(
                rules.contains("requires_sync_bridge"),
                "{} {:?} should contain truthful indexing-transport guidance",
                editor.display_name(),
                mode
            );
            assert!(
                !rules.contains("NEVER claim that transport mode"),
                "{} {:?} should not promise local-path ingest on remote transports",
                editor.display_name(),
                mode
            );
            assert!(
                rules.contains("hosted MCP") && rules.contains("sync bridge"),
                "{} {:?} should keep hosted MCP as the preferred transport",
                editor.display_name(),
                mode
            );
        }
    }

    #[test]
    fn test_bootstrap_mode_includes_plan_mode_guardrail() {
        let rules = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Bootstrap);
        assert!(rules.contains("Entering plan mode does NOT bypass search-first"));
        assert!(rules.contains("Explore, Task subagents"));
    }

    #[test]
    fn test_no_hooks_supplement_includes_two_phase_search_pattern() {
        let rules = generate_rule_content(&Editor::Codex, None, None, RulesMode::Full);
        assert!(rules.contains("Two-Phase Search Pattern (for precision)"));
        assert!(rules.contains("search(mode=\"refactor\", query=\"SymbolName\""));
        assert!(rules.contains("search(mode=\"exhaustive\", query=\"symbol_or_text\")"));
    }

    #[test]
    fn test_replace_contextstream_block_malformed_begin_without_end() {
        // BEGIN marker exists but END marker is missing (malformed)
        let existing = format!(
            "User content before\n{}\nOrphaned ContextStream content\nMore orphaned stuff",
            CONTEXTSTREAM_START
        );
        let error = replace_contextstream_block(&existing, "new block")
            .expect_err("an unclosed marker must fail closed");

        assert!(error.to_string().contains("no closing marker"));
    }

    #[test]
    fn malformed_rules_file_is_unchanged_by_install_and_uninstall() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("CLAUDE.md");
        let existing = format!(
            "{}\nPossibly user-authored content after a stray marker.\n",
            CONTEXTSTREAM_START
        );
        std::fs::write(&path, &existing).expect("seed malformed rules");

        let install_error =
            write_contextstream_block_to_path(&path, "new block", true).expect_err("install");
        assert!(install_error.to_string().contains("no closing marker"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert!(!safe_edit::backup_path(&path).unwrap().exists());

        let uninstall_error =
            remove_contextstream_from_path(&path).expect_err("uninstall must also fail closed");
        assert!(uninstall_error.to_string().contains("no closing marker"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert!(!safe_edit::backup_path(&path).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn rules_uninstall_refuses_a_symlinked_recovery_backup() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("CLAUDE.md");
        let live = format!(
            "# User rules\n\n{}\n{} 0123456789abcdef -->\nmanaged\n{}\n",
            CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
        );
        std::fs::write(&path, &live).expect("seed live rules");
        let unrelated = temp.path().join("unrelated.md");
        std::fs::write(&unrelated, "# unrelated user file\n").expect("seed unrelated file");
        let backup = safe_edit::backup_path(&path).expect("backup path");
        symlink(&unrelated, &backup).expect("plant backup symlink");

        let error =
            remove_contextstream_from_path(&path).expect_err("backup symlink must fail closed");

        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), live);
        assert_eq!(
            std::fs::read_to_string(&unrelated).unwrap(),
            "# unrelated user file\n"
        );
    }

    #[test]
    fn test_replace_contextstream_block_idempotent() {
        // Running replace twice must preserve every byte. Generated blocks end
        // in a newline while the scanner leaves the prior block's following
        // newline outside its bounds; concatenating both used to grow one blank
        // line (and create a backup) on every refresh.
        let initial = "User rules\nSome custom content";
        let block = stamp_block_with_canonical_rules_hash(&format!(
            "\n{}\nContextStream rules\n{}\n",
            CONTEXTSTREAM_START, CONTEXTSTREAM_END
        ));

        let first = replace_contextstream_block(initial, &block).expect("first replacement");
        let second = replace_contextstream_block(&first, &block).expect("second replacement");

        assert_eq!(first, second, "replacing twice must be byte-idempotent");
        // Both runs must contain exactly one copy of the block and user content
        assert_eq!(
            second.matches(CONTEXTSTREAM_START).count(),
            1,
            "should contain exactly one BEGIN marker"
        );
        assert_eq!(
            second.matches(CONTEXTSTREAM_END).count(),
            1,
            "should contain exactly one END marker"
        );
        assert!(second.contains("User rules"));
        assert!(second.contains("Some custom content"));
    }

    #[test]
    fn test_cursor_supplement_only_for_cursor() {
        let cursor = generate_rule_content(&Editor::Cursor, None, None, RulesMode::Full);
        let claude = generate_rule_content(&Editor::ClaudeCode, None, None, RulesMode::Full);

        assert!(
            cursor.contains("Cursor-Specific Rules"),
            "Cursor should get the Cursor supplement"
        );
        assert!(
            !claude.contains("Cursor-Specific Rules"),
            "Claude Code should NOT get the Cursor supplement"
        );
    }

    #[test]
    fn test_codex_skip_removes_existing_agents_contextstream_block() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        // Simulate another editor already having project-level ContextStream rules.
        let windsurf_path = project
            .join(".windsurf")
            .join("rules")
            .join("contextstream.md");
        std::fs::create_dir_all(windsurf_path.parent().expect("windsurf parent")).expect("mkdirs");
        std::fs::write(
            &windsurf_path,
            format!(
                "{}\nWindsurf content\n{}",
                CONTEXTSTREAM_START, CONTEXTSTREAM_END
            ),
        )
        .expect("write windsurf rules");

        // Existing AGENTS.md with legacy markers should be auto-cleaned on skip.
        let agents_path = project.join("AGENTS.md");
        std::fs::write(
            &agents_path,
            format!(
                "{}\nStale codex rules\n{}\nUser notes",
                LEGACY_START, LEGACY_END
            ),
        )
        .expect("write agents");

        write_project_rules(&Editor::Codex, project, None, None, Some("project")).expect("write");

        let updated = std::fs::read_to_string(&agents_path).expect("read agents");
        assert!(
            !updated.contains(LEGACY_START),
            "legacy start marker should be removed"
        );
        assert!(
            !updated.contains(LEGACY_END),
            "legacy end marker should be removed"
        );
        assert!(
            updated.contains("User notes"),
            "non-ContextStream content stays"
        );
    }

    #[test]
    fn test_opencode_skip_removes_existing_agents_contextstream_block() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        // Simulate another editor already having project-level ContextStream rules.
        let windsurf_path = project
            .join(".windsurf")
            .join("rules")
            .join("contextstream.md");
        std::fs::create_dir_all(windsurf_path.parent().expect("windsurf parent")).expect("mkdirs");
        std::fs::write(
            &windsurf_path,
            format!(
                "{}\nWindsurf content\n{}",
                CONTEXTSTREAM_START, CONTEXTSTREAM_END
            ),
        )
        .expect("write windsurf rules");

        // Existing AGENTS.md with legacy markers should be auto-cleaned on skip.
        let agents_path = project.join("AGENTS.md");
        std::fs::write(
            &agents_path,
            format!(
                "{}\nStale opencode rules\n{}\nUser notes",
                LEGACY_START, LEGACY_END
            ),
        )
        .expect("write agents");

        write_project_rules(&Editor::OpenCode, project, None, None, Some("project"))
            .expect("write");

        let updated = std::fs::read_to_string(&agents_path).expect("read agents");
        assert!(
            !updated.contains(LEGACY_START),
            "legacy start marker should be removed"
        );
        assert!(
            !updated.contains(LEGACY_END),
            "legacy end marker should be removed"
        );
        assert!(
            updated.contains("User notes"),
            "non-ContextStream content stays"
        );
    }

    #[test]
    fn test_opencode_writes_agents_when_only_cursor_has_rules() {
        // Regression: previously the dedup skipped AGENTS.md whenever any of
        // [Windsurf, Cursor, ClaudeCode, Cline, KiloCode, RooCode] had rules.
        // Cursor reads its own `.cursor/rules/*.mdc`, not AGENTS.md, so OpenCode
        // must still receive its AGENTS.md when only Cursor is present.
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        let cursor_path = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        std::fs::create_dir_all(cursor_path.parent().expect("cursor rules parent"))
            .expect("mkdirs");
        std::fs::write(
            &cursor_path,
            format!(
                "{}\nCursor content\n{}",
                CONTEXTSTREAM_START, CONTEXTSTREAM_END
            ),
        )
        .expect("write cursor rules");

        write_project_rules(&Editor::OpenCode, project, None, None, Some("project"))
            .expect("write");

        let agents = project.join("AGENTS.md");
        assert!(
            agents.exists(),
            "AGENTS.md must be written even when .cursorrules already exists"
        );
        let content = std::fs::read_to_string(&agents).expect("read agents");
        assert!(
            content.contains(CONTEXTSTREAM_START),
            "AGENTS.md should contain ContextStream rules"
        );
    }

    #[test]
    fn test_codex_writes_agents_when_only_claude_has_rules() {
        // Regression: ClaudeCode reads CLAUDE.md only, not AGENTS.md, so Codex
        // must still receive its AGENTS.md when only ClaudeCode is present.
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        let claude_path = project.join("CLAUDE.md");
        std::fs::write(
            &claude_path,
            format!(
                "{}\nClaude content\n{}",
                CONTEXTSTREAM_START, CONTEXTSTREAM_END
            ),
        )
        .expect("write claude rules");

        write_project_rules(&Editor::Codex, project, None, None, Some("project")).expect("write");

        let agents = project.join("AGENTS.md");
        assert!(
            agents.exists(),
            "AGENTS.md must be written even when CLAUDE.md already exists"
        );
        let content = std::fs::read_to_string(&agents).expect("read agents");
        assert!(
            content.contains(CONTEXTSTREAM_START),
            "AGENTS.md should contain ContextStream rules"
        );
    }

    #[test]
    fn codex_rule_write_fails_closed_when_windsurf_rules_cannot_be_read() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let windsurf_primary = project
            .join(".windsurf")
            .join("rules")
            .join("contextstream.md");

        // Directory-style rule targets resolve one level deeper. Making that
        // resolved target a directory reliably produces a read error on every
        // platform without relying on permission semantics.
        std::fs::create_dir_all(windsurf_primary.join("contextstream.md"))
            .expect("create unreadable rule shape");

        let error = write_project_rules(&Editor::Codex, project, None, None, None)
            .expect_err("an unreadable duplicate-prevention input must abort");

        assert!(
            error
                .to_string()
                .contains("Could not read Windsurf rules file"),
            "unexpected error: {error:#}"
        );
        assert!(
            !project.join("AGENTS.md").exists(),
            "Codex rules were written after the Windsurf safety check failed"
        );
    }

    #[test]
    fn test_mode_for_editor_uses_tier_mapping() {
        assert_eq!(mode_for_editor(&Editor::ClaudeCode), RulesMode::Bootstrap);
        assert_eq!(mode_for_editor(&Editor::Cline), RulesMode::Bootstrap);
        assert_eq!(mode_for_editor(&Editor::KiloCode), RulesMode::Minimal);
        assert_eq!(mode_for_editor(&Editor::RooCode), RulesMode::Minimal);
        assert_eq!(mode_for_editor(&Editor::Copilot), RulesMode::Full);
        assert_eq!(mode_for_editor(&Editor::Codex), RulesMode::Full);
        assert_eq!(mode_for_editor(&Editor::OpenCode), RulesMode::Full);
        assert_eq!(mode_for_editor(&Editor::Aider), RulesMode::Full);
    }

    #[test]
    fn test_write_project_rules_updates_existing_cursor_legacy_rule_file() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let legacy = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.md");
        std::fs::create_dir_all(legacy.parent().expect("legacy parent")).expect("mkdirs");
        std::fs::write(
            &legacy,
            format!(
                "{}\n# Workspace: Legacy\n# Workspace ID: legacy-id\n# ContextStream Rules\n**MANDATORY STARTUP:** legacy\n{}",
                CONTEXTSTREAM_START, CONTEXTSTREAM_END
            ),
        )
        .expect("write legacy file");

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write rules");

        let updated = std::fs::read_to_string(&legacy).expect("read legacy updated");
        assert!(!updated.contains("legacy"));
        assert!(updated.contains(mcp_types::HARNESS_TEACHING_VERSION));
    }

    #[test]
    fn test_write_project_rules_cursor_writes_mdc_with_frontmatter() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write cursor rules");

        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        let content = std::fs::read_to_string(&mdc).expect("read cursor mdc");

        assert!(
            content.starts_with("---\n"),
            "mdc must start with YAML frontmatter"
        );
        assert!(
            content.contains("alwaysApply: true"),
            "mdc frontmatter must set alwaysApply: true"
        );
        assert!(content.contains(CONTEXTSTREAM_START));
        assert!(content.contains(mcp_types::HARNESS_TEACHING_VERSION));
        // Cursor uses raw (unprefixed) tool names, never the Claude prefix.
        assert!(!content.contains("mcp__contextstream__"));
    }

    #[test]
    fn test_write_project_rules_cursor_mdc_frontmatter_idempotent() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        for _ in 0..3 {
            write_project_rules(
                &Editor::Cursor,
                project,
                Some("ws"),
                Some("Workspace"),
                Some("proj"),
            )
            .expect("write cursor rules");
        }

        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        let content = std::fs::read_to_string(&mdc).expect("read cursor mdc");

        assert_eq!(
            content.matches("alwaysApply: true").count(),
            1,
            "frontmatter must not duplicate across refreshes"
        );
        assert_eq!(
            content.matches(CONTEXTSTREAM_START).count(),
            1,
            "exactly one ContextStream block after refreshes"
        );
    }

    #[test]
    fn cursor_mdc_preserves_custom_frontmatter_and_uninstalls_exactly() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        std::fs::create_dir_all(mdc.parent().unwrap()).unwrap();
        let original = concat!(
            "---\n",
            "description: Team-owned rule\n",
            "alwaysApply: false\n",
            "globs: [\"src/**\"]\n",
            "---\n\n",
            "# User workflow\n",
            "Keep this text byte-for-byte.\n",
        );
        std::fs::write(&mdc, original).unwrap();

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("install cursor rules");

        let installed = std::fs::read_to_string(&mdc).unwrap();
        assert!(installed.starts_with(
            "---\ndescription: Team-owned rule\nalwaysApply: false\nglobs: [\"src/**\"]\n---\n\n"
        ));
        assert!(installed.contains("# User workflow\nKeep this text byte-for-byte.\n"));
        assert!(installed.contains(CONTEXTSTREAM_START));

        assert!(
            remove_contextstream_from_rules(&Editor::Cursor, Some(project))
                .expect("uninstall cursor rules")
        );
        assert_eq!(std::fs::read_to_string(&mdc).unwrap(), original);
        assert!(!safe_edit::backup_path(&mdc).unwrap().exists());
    }

    #[test]
    fn cursor_mdc_surgical_uninstall_preserves_post_install_user_edits() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        std::fs::create_dir_all(mdc.parent().unwrap()).unwrap();
        let original =
            "---\ndescription: Team-owned rule\ncustom: yes\n---\n\n# Original user rule\n";
        std::fs::write(&mdc, original).unwrap();

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("install cursor rules");
        let mut edited = std::fs::read_to_string(&mdc).unwrap();
        edited.push_str("\n# Added after install\n");
        std::fs::write(&mdc, edited).unwrap();

        remove_contextstream_from_rules(&Editor::Cursor, Some(project))
            .expect("surgical uninstall");
        let cleaned = std::fs::read_to_string(&mdc).unwrap();
        assert!(cleaned.starts_with("---\ndescription: Team-owned rule\ncustom: yes\n---\n\n"));
        assert!(cleaned.contains("# Original user rule"));
        assert!(cleaned.contains("# Added after install"));
        assert!(!cleaned.contains(CONTEXTSTREAM_START));
        assert!(safe_edit::backup_path(&mdc).unwrap().exists());
    }

    #[test]
    fn generated_cursor_mdc_uninstalls_without_frontmatter_or_backup_debris() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("install cursor rules");
        remove_contextstream_from_rules(&Editor::Cursor, Some(project))
            .expect("uninstall cursor rules");

        assert!(!mdc.exists());
        assert!(!safe_edit::backup_path(&mdc).unwrap().exists());
    }

    #[test]
    fn test_write_project_rules_cursor_strips_legacy_cursorrules_block() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        let cursorrules = project.join(".cursorrules");
        std::fs::write(
            &cursorrules,
            format!(
                "# My rules\n{}\n{} 0123456789abcdef -->\nstale block\n{}\n\nkeep this\n",
                CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
            ),
        )
        .expect("write .cursorrules");

        write_project_rules(
            &Editor::Cursor,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write cursor rules");

        // Primary `.mdc` is created.
        assert!(project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc")
            .exists());

        // Legacy `.cursorrules` block is stripped, unmanaged content preserved,
        // and the file is never turned back into a managed rules target.
        let leftover = std::fs::read_to_string(&cursorrules).unwrap_or_default();
        assert!(!leftover.contains(CONTEXTSTREAM_START));
        assert!(!leftover.contains("stale block"));
        assert!(leftover.contains("keep this"));
    }

    #[test]
    fn test_write_project_rules_for_aider_writes_read_pointer_and_shared_rules() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        write_project_rules(
            &Editor::Aider,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write aider rules");

        let aider_path = project.join(".aider.conf.yml");
        let aider = std::fs::read_to_string(&aider_path).expect("read aider config");
        assert!(aider.contains("read:"));
        assert!(aider.contains(".contextstream/rules.md"));
        assert!(aider.contains(CONTEXTSTREAM_START));
        assert!(aider.contains(CONTEXTSTREAM_END));

        let shared = project.join(".contextstream").join("rules.md");
        let shared_content = std::fs::read_to_string(&shared).expect("read shared rules");
        assert!(shared_content.contains(CONTEXTSTREAM_START));
        assert!(shared_content.contains(mcp_types::HARNESS_TEACHING_VERSION));
        assert!(shared_content.contains("No Hooks Available"));
    }

    #[test]
    fn test_windsurf_rules_have_always_on_frontmatter_once() {
        let rules = generate_rule_content(
            &Editor::Windsurf,
            Some("test-id"),
            Some("Test"),
            RulesMode::Bootstrap,
        );
        assert!(rules.starts_with(WINDSURF_ALWAYS_ON_FRONTMATTER));
        assert_eq!(
            rules.matches(WINDSURF_ALWAYS_ON_FRONTMATTER).count(),
            1,
            "frontmatter should appear exactly once"
        );
    }

    #[test]
    fn test_write_project_rules_migrates_legacy_windsurfrules() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let legacy = project.join(".windsurfrules");
        std::fs::write(
            &legacy,
            format!(
                "{}\n{} 0123456789abcdef -->\nlegacy contextstream\n{}\ncustom user content",
                CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
            ),
        )
        .expect("write legacy windsurfrules");

        write_project_rules(
            &Editor::Windsurf,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write windsurf project rules");

        let new_rules = project
            .join(".windsurf")
            .join("rules")
            .join("contextstream.md");
        let new_content = std::fs::read_to_string(&new_rules).expect("read new windsurf rules");
        assert!(new_content.starts_with(WINDSURF_ALWAYS_ON_FRONTMATTER));
        assert!(new_content.contains(CONTEXTSTREAM_START));
        assert!(new_content.contains(CONTEXTSTREAM_END));

        let migrated_legacy = std::fs::read_to_string(&legacy).expect("read legacy after migrate");
        assert!(
            !migrated_legacy.contains(CONTEXTSTREAM_START),
            "legacy file should no longer contain managed ContextStream block"
        );
        assert!(
            !migrated_legacy.contains(CONTEXTSTREAM_END),
            "legacy file should no longer contain managed ContextStream block"
        );
        assert!(
            migrated_legacy.contains("custom user content"),
            "legacy file should preserve unrelated user content"
        );
    }

    #[test]
    fn test_write_project_rules_handles_directory_style_target_for_cline() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let cline_rules_dir = project.join(".clinerules");
        std::fs::create_dir_all(&cline_rules_dir).expect("create .clinerules directory");

        write_project_rules(
            &Editor::Cline,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write cline project rules");

        let managed = cline_rules_dir.join("contextstream.md");
        let content = std::fs::read_to_string(&managed).expect("read managed rule file");
        assert!(content.contains(CONTEXTSTREAM_START));
        assert!(content.contains(CONTEXTSTREAM_END));
    }

    #[test]
    fn test_write_project_rules_for_copilot_writes_skill_file() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        write_project_rules(
            &Editor::Copilot,
            project,
            Some("ws"),
            Some("Workspace"),
            Some("proj"),
        )
        .expect("write copilot rules");

        let instructions = project.join(".github").join("copilot-instructions.md");
        let instructions_content =
            std::fs::read_to_string(&instructions).expect("read copilot instructions");
        assert!(instructions_content.contains(CONTEXTSTREAM_START));
        assert!(instructions_content.contains(CONTEXTSTREAM_END));

        let skill = project
            .join(".github")
            .join("skills")
            .join("contextstream-workflow")
            .join("SKILL.md");
        let skill_content = std::fs::read_to_string(&skill).expect("read copilot skill file");
        assert!(skill_content.contains("name: contextstream-workflow"));
        assert!(skill_content.contains("Session Lifecycle"));
        assert!(skill_content
            .contains("check whether ContextStream already surfaced relevant skills, docs, lessons, or decisions"));
        assert!(skill_content
            .contains("Reuse the current `project_id` returned by `init` or `context`"));
        assert!(skill_content.contains("search(mode=\"auto\", query=\"...\")"));
        assert!(skill_content.contains(COPILOT_SKILL_HASH_MARKER_PREFIX));
        assert!(copilot_skill_hash_marker_is_valid(&skill_content));
        assert!(copilot_skill_is_owned(&skill_content));
    }

    fn replace_copilot_hash_marker(mut content: String, replacement: &str) -> String {
        let marker_start = content
            .find(COPILOT_SKILL_HASH_MARKER_PREFIX)
            .expect("current skill hash marker");
        let marker_end = content[marker_start..]
            .find(COPILOT_SKILL_HASH_MARKER_SUFFIX)
            .map(|offset| marker_start + offset + COPILOT_SKILL_HASH_MARKER_SUFFIX.len())
            .expect("current skill hash marker suffix");
        content.replace_range(marker_start..marker_end, replacement);
        content
    }

    fn v0_5_86_copilot_skill_content() -> String {
        replace_copilot_hash_marker(
            canonical_copilot_skill_content(),
            COPILOT_SKILL_LEGACY_MARKER,
        )
    }

    fn v0_5_85_copilot_skill_content() -> String {
        v0_5_86_copilot_skill_content().replace(
            "`entity(kind=\"handoff\", action=\"create\", body={\"title\":\"...\",\"summary\":\"...\",\"scope\":\"...\",\"next_steps\":[...]}, workspace_id=\"<current_workspace_id>\", project_id=\"<current_project_id>\")`",
            "`entity(kind=\"handoff\", action=\"create\", body={\"title\":\"...\",\"summary\":\"...\",\"scope\":\"...\",\"next_steps\":[...]})`",
        )
    }

    #[test]
    fn copilot_skill_self_verifying_marker_detects_user_edits() {
        let current = canonical_copilot_skill_content();
        assert!(copilot_skill_hash_marker_is_valid(&current));

        let crlf = current.replace('\n', "\r\n");
        assert!(copilot_skill_hash_marker_is_valid(&crlf));
        assert!(copilot_skill_is_owned(&crlf));

        let edited = current.replacen(
            "# ContextStream Workflow Skill",
            "# My Customized ContextStream Workflow",
            1,
        );
        assert!(!copilot_skill_hash_marker_is_valid(&edited));
        assert!(!copilot_skill_is_owned(&edited));
    }

    #[test]
    fn copilot_skill_previous_releases_are_recognized_and_upgraded() {
        let v0_5_85 = v0_5_85_copilot_skill_content();
        assert_eq!(
            sha256_hex(&[v0_5_85.as_bytes()]),
            "f394bb3bd849884ee4269acfed10c20f4f7ba8b1e4018104ad4c0451b53928f7"
        );
        assert!(copilot_skill_is_owned(&v0_5_85));

        let v0_5_86 = v0_5_86_copilot_skill_content();
        assert_eq!(
            sha256_hex(&[v0_5_86.as_bytes()]),
            "4571df4c51a7347e9617dc12b87810d6f87ce84fbfef36a3fd01f6d517b0d758"
        );
        assert!(copilot_skill_is_owned(&v0_5_86));

        let temp = tempdir().expect("tempdir");
        let skill = temp.path().join(COPILOT_SKILL_PATH);
        std::fs::create_dir_all(skill.parent().expect("skill parent")).expect("create skill dir");
        std::fs::write(&skill, v0_5_85).expect("seed v0.5.85 skill");

        write_project_rules(
            &Editor::Copilot,
            temp.path(),
            Some("workspace-id"),
            Some("Workspace"),
            Some("project"),
        )
        .expect("update Copilot rules with previous-release skill");

        assert_eq!(
            std::fs::read_to_string(&skill).expect("read upgraded skill"),
            canonical_copilot_skill_content()
        );
    }

    #[test]
    fn copilot_skill_marker_does_not_authorize_overwriting_user_edits() {
        let temp = tempdir().expect("tempdir");
        let skill = temp.path().join(COPILOT_SKILL_PATH);
        std::fs::create_dir_all(skill.parent().expect("skill parent")).expect("create skill dir");
        let user_content = format!(
            "---\nname: contextstream-workflow\n---\n\n{COPILOT_SKILL_LEGACY_MARKER}\n\nUser-authored workflow.\n"
        );
        std::fs::write(&skill, &user_content).expect("seed user skill");

        let error = write_copilot_skill_file(temp.path())
            .expect_err("a marker alone must not imply whole-file ownership");

        assert!(error
            .to_string()
            .contains("Refusing to overwrite user-owned"));
        assert_eq!(
            std::fs::read_to_string(&skill).expect("read preserved skill"),
            user_content
        );
        assert!(!safe_edit::backup_path(&skill).unwrap().exists());
    }

    #[test]
    fn copilot_skill_uninstall_deletes_only_exact_owned_content_without_backup() {
        let temp = tempdir().expect("tempdir");
        let skill = temp.path().join(COPILOT_SKILL_PATH);
        std::fs::create_dir_all(skill.parent().expect("skill parent")).expect("create skill dir");
        std::fs::write(&skill, canonical_copilot_skill_content()).expect("seed owned skill");

        assert!(
            remove_contextstream_from_rules(&Editor::Copilot, Some(temp.path()))
                .expect("uninstall owned skill")
        );
        assert!(!skill.exists());
        assert!(!safe_edit::backup_path(&skill).unwrap().exists());

        let modified = format!(
            "{}\nUser customization.\n",
            canonical_copilot_skill_content().trim_end()
        );
        std::fs::write(&skill, &modified).expect("seed modified skill");
        remove_contextstream_from_rules(&Editor::Copilot, Some(temp.path()))
            .expect("uninstall preserves modified skill");
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), modified);
    }

    #[test]
    fn copilot_skill_uninstall_validates_recovery_before_deleting_live_file() {
        let temp = tempdir().expect("tempdir");
        let skill = temp.path().join(COPILOT_SKILL_PATH);
        std::fs::create_dir_all(skill.parent().expect("skill parent")).expect("create skill dir");
        let owned = canonical_copilot_skill_content();
        std::fs::write(&skill, &owned).expect("seed owned skill");
        let backup = safe_edit::backup_path(&skill).expect("backup path");
        std::fs::write(&backup, "user or corrupt recovery content\n").expect("seed bad backup");

        let error = remove_contextstream_from_rules(&Editor::Copilot, Some(temp.path()))
            .expect_err("unrecognized recovery state must fail closed");

        assert!(error.to_string().contains("not a recognized"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), owned);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "user or corrupt recovery content\n"
        );
    }

    #[test]
    fn test_infer_workspace_identity_prefers_existing_rule_headers() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        let global_rules = temp.path().join(".codex").join("AGENTS.md");
        std::fs::create_dir_all(global_rules.parent().expect("parent")).expect("mkdirs");
        std::fs::write(
            &global_rules,
            format!(
                "{}\n{} 0123456789abcdef -->\n# Workspace: Engineering\n# Workspace ID: 11111111-2222-4333-8444-555555555555\n{}\n",
                CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
            ),
        )
        .expect("write rules");

        let (workspace_id, workspace_name) =
            infer_workspace_identity_from_existing_rules(&[Editor::Codex], None, true, false);
        assert_eq!(
            workspace_id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(workspace_name.as_deref(), Some("Engineering"));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn inferred_workspace_identity_never_reads_an_unselected_editor() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        let claude_rules = temp.path().join(".claude").join("CLAUDE.md");
        let codex_rules = temp.path().join(".codex").join("AGENTS.md");
        for (path, name, id) in [
            (&claude_rules, "Wrong Claude workspace", "wrong-workspace"),
            (
                &codex_rules,
                "Selected Codex workspace",
                "selected-workspace",
            ),
        ] {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
            std::fs::write(
                path,
                format!(
                    "{}\n{} 0123456789abcdef -->\n# Workspace: {name}\n# Workspace ID: {id}\n{}\n",
                    CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
                ),
            )
            .expect("write rules");
        }

        let (workspace_id, workspace_name) =
            infer_workspace_identity_from_existing_rules(&[Editor::Codex], None, true, false);
        assert_eq!(workspace_id.as_deref(), Some("selected-workspace"));
        assert_eq!(workspace_name.as_deref(), Some("Selected Codex workspace"));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_remove_contextstream_from_directory_style_target() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let cline_rules_dir = project.join(".clinerules");
        std::fs::create_dir_all(&cline_rules_dir).expect("create .clinerules directory");
        let managed = cline_rules_dir.join("contextstream.md");

        std::fs::write(
            &managed,
            format!(
                "{}\n{} 0123456789abcdef -->\nmanaged\n{}\nextra content",
                CONTEXTSTREAM_START, RULES_HASH_MARKER_PREFIX, CONTEXTSTREAM_END
            ),
        )
        .expect("write managed file");

        let removed =
            remove_contextstream_from_rules(&Editor::Cline, Some(project)).expect("remove block");
        assert!(removed, "expected ContextStream block removal to succeed");
        let cleaned = std::fs::read_to_string(&managed).expect("read cleaned file");
        assert!(!cleaned.contains(CONTEXTSTREAM_START));
        assert!(!cleaned.contains(CONTEXTSTREAM_END));
        assert!(cleaned.contains("extra content"));
    }
}
