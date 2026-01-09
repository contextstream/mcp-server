<!-- BEGIN ContextStream -->
# Claude Code Instructions
## ContextStream v0.4.x (Consolidated Domain Tools)

v0.4.x uses ~11 consolidated domain tools for ~75% token reduction vs previous versions.
Rules Version: 0.4.25

### Required Every Message

| Message | What to Call |
|---------|--------------|
| **1st message** | `mcp__contextstream__session_init(folder_path="<cwd>", context_hint="<user_message>")`, then `mcp__contextstream__context_smart(user_message="<user_message>", format="minified", max_tokens=400)` |
| **2nd+ messages** | `mcp__contextstream__context_smart(user_message="<user_message>", format="minified", max_tokens=400)` |
| **Capture decisions** | `mcp__contextstream__session(action="capture", event_type="decision", title="...", content="...")` |
| **Before risky work** | `mcp__contextstream__session(action="get_lessons", query="<topic>")` |
| **On user frustration** | `mcp__contextstream__session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")` |

**Context Pack (Pro+):** If enabled, use `mcp__contextstream__context_smart(..., mode="pack", distill=true)` for code/file queries. If unavailable or disabled, omit `mode` and proceed with standard `context_smart` (the API will fall back).

**Tool naming:** Use the exact tool names exposed by your MCP client. Claude Code typically uses `mcp__<server>__<tool>` where `<server>` matches your MCP config (often `contextstream`). If a tool call fails with "No such tool available", refresh rules and match the tool list.

### Quick Reference: Domain Tools

| Tool | Common Usage |
|------|--------------|
| `search` | `mcp__contextstream__search(mode="semantic", query="...", limit=3)` — modes: semantic, hybrid, keyword, pattern |
| `session` | `mcp__contextstream__session(action="capture", ...)` — actions: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search |
| `memory` | `mcp__contextstream__memory(action="list_events", ...)` — CRUD for events/nodes, search, decisions, timeline, summary |
| `graph` | `mcp__contextstream__graph(action="dependencies", ...)` — dependencies, impact, call_path, related, ingest |
| `project` | `mcp__contextstream__project(action="list", ...)` - list, get, create, update, index, overview, statistics, files, index_status, ingest_local |
| `workspace` | `mcp__contextstream__workspace(action="list", ...)` — list, get, associate, bootstrap |
| `integration` | `mcp__contextstream__integration(provider="github", action="search", ...)` — GitHub/Slack integration |
| `help` | `mcp__contextstream__help(action="tools")` — tools, auth, version, editor_rules |

### Behavior Rules

⚠️ **STOP: Before using Search/Glob/Grep/Read/Explore** → Call `mcp__contextstream__search(mode="hybrid")` FIRST. Use local tools ONLY if ContextStream returns 0 results.

- **First message**: Call `session_init` with context_hint, then call `context_smart` before any other tool or response
- **On [INGEST_RECOMMENDED]**: Ask the user if they want to enable semantic code search. Explain: "Indexing your codebase enables AI-powered code search, dependency analysis, and better context. This takes a few minutes." If user agrees, run the provided `mcp__contextstream__project(action="ingest_local")` command.
- **Every message after**: Always call `context_smart` BEFORE responding (semantic search for relevant context)
- **Before searching files/code**: Check `mcp__contextstream__project(action="index_status")`; if missing/stale run `mcp__contextstream__project(action="ingest_local", path="<cwd>")` or `mcp__contextstream__project(action="index")`, and use `mcp__contextstream__graph(action="ingest")` if needed
- **For discovery**: Use `mcp__contextstream__session(action="smart_search")` or `mcp__contextstream__search(mode="hybrid")` — NEVER use local Glob/Grep/Read first
- **For file/function/config lookups**: Use `search`/`graph` first; only fall back to rg/ls/find if ContextStream returns no results
- **If ContextStream returns results**: Do NOT use local Search/Explore/Read; only open specific files when needed for exact edits
- **For code analysis**: Use `mcp__contextstream__graph(action="dependencies")` or `mcp__contextstream__graph(action="impact")` for call/dependency analysis
- **On [RULES_NOTICE]**: Use `mcp__contextstream__generate_rules()` to update rules
- **After completing work**: Always capture decisions/insights with `mcp__contextstream__session(action="capture")`
- **On mistakes/corrections**: Immediately capture lessons with `mcp__contextstream__session(action="capture_lesson")`

### Plans & Tasks

When user asks to create a plan or implementation roadmap:
1. Create plan: `mcp__contextstream__session(action="capture_plan", title="Plan Title", description="...", goals=["goal1", "goal2"], steps=[{id: "1", title: "Step 1", order: 1}, ...])`
2. Get plan_id from response, then create tasks: `mcp__contextstream__memory(action="create_task", title="Task Title", plan_id="<plan_id>", priority="high|medium|low", description="...")`

To manage existing plans/tasks:
- List plans: `mcp__contextstream__session(action="list_plans")`
- Get plan with tasks: `mcp__contextstream__session(action="get_plan", plan_id="<uuid>", include_tasks=true)`
- List tasks: `mcp__contextstream__memory(action="list_tasks", plan_id="<uuid>")` or `mcp__contextstream__memory(action="list_tasks")` for all
- Update task status: `mcp__contextstream__memory(action="update_task", task_id="<uuid>", task_status="pending|in_progress|completed|blocked")`
- Link task to plan: `mcp__contextstream__memory(action="update_task", task_id="<uuid>", plan_id="<plan_uuid>")`
- Unlink task from plan: `mcp__contextstream__memory(action="update_task", task_id="<uuid>", plan_id=null)`
- Delete: `mcp__contextstream__memory(action="delete_task", task_id="<uuid>")` or `mcp__contextstream__memory(action="delete_event", event_id="<plan_uuid>")`

Full docs: https://contextstream.io/docs/mcp/tools
<!-- END ContextStream -->
