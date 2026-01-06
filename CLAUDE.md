<!-- BEGIN ContextStream -->
# Workspace: Maker
# Project: mcp-server
# Workspace ID: ecb7007a-9271-4066-86eb-2a35da261d54

# Claude Code Instructions
## ContextStream v0.4.x (Consolidated Domain Tools)

v0.4.x uses ~11 consolidated domain tools for ~75% token reduction vs previous versions.
Rules Version: 0.4.16

### Required Every Message

| Message | What to Call |
|---------|--------------|
| **1st message** | `mcp__contextstream__session_init(folder_path="<cwd>", context_hint="<user_message>")` |
| **2nd+ messages** | `mcp__contextstream__context_smart(user_message="<user_message>", format="minified", max_tokens=400)` |
| **Capture decisions** | `mcp__contextstream__session(action="capture", event_type="decision", title="...", content="...")` |
| **Before risky work** | `mcp__contextstream__session(action="get_lessons", query="<topic>")` |
| **On user frustration** | `mcp__contextstream__session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")` |

**Context Pack (Pro+):** If enabled, use `mcp__contextstream__context_smart(..., mode="pack", distill=true)` for code/file queries. If unavailable, omit `mode`.

### Quick Reference: Domain Tools

| Tool | Common Usage |
|------|--------------|
| `mcp__contextstream__search` | `mcp__contextstream__search(mode="semantic", query="...")` — modes: semantic, hybrid, keyword, pattern |
| `mcp__contextstream__session` | `mcp__contextstream__session(action="capture", ...)` — actions: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search |
| `mcp__contextstream__memory` | `mcp__contextstream__memory(action="list_events", ...)` — CRUD for events/nodes, mcp__contextstream__search, decisions, timeline, summary |
| `mcp__contextstream__graph` | `mcp__contextstream__graph(action="dependencies", ...)` — dependencies, impact, call_path, related, ingest |
| `mcp__contextstream__project` | `mcp__contextstream__project(action="list", ...)` - list, get, create, update, index, overview, statistics, files, index_status, ingest_local |
| `mcp__contextstream__workspace` | `mcp__contextstream__workspace(action="list", ...)` — list, get, associate, bootstrap |
| `mcp__contextstream__integration` | `mcp__contextstream__integration(provider="github", action="mcp__contextstream__search", ...)` — GitHub/Slack mcp__contextstream__integration |
| `mcp__contextstream__help` | `mcp__contextstream__help(action="tools")` — tools, auth, version, editor_rules |

### Behavior Rules

- **First message**: Always call `mcp__contextstream__session_init` with context_hint
- **Every message after**: Always call `mcp__contextstream__context_smart` BEFORE responding (semantic mcp__contextstream__search for relevant context)
- **Before searching files/code**: Check `mcp__contextstream__project(action="index_status")`; if missing/stale run `mcp__contextstream__project(action="ingest_local", path="<cwd>")` or `mcp__contextstream__project(action="index")`, and use `mcp__contextstream__graph(action="ingest")` if needed
- **For discovery**: Use `mcp__contextstream__session(action="smart_search")` or `mcp__contextstream__search(mode="hybrid")` before any local repo scans
- **For file/function/config lookups**: Use `mcp__contextstream__search`/`mcp__contextstream__graph` first; only fall back to rg/ls/find if ContextStream returns no results
- **For code analysis**: Use `mcp__contextstream__graph(action="dependencies")` or `mcp__contextstream__graph(action="impact")` for call/dependency analysis
- **On [RULES_NOTICE]**: Use `mcp__contextstream__generate_rules()` to update rules
- **After completing work**: Always capture decisions/insights with `mcp__contextstream__session(action="capture")`
- **On mistakes/corrections**: Immediately capture lessons with `mcp__contextstream__session(action="capture_lesson")`

Full docs: https://contextstream.io/docs/mcp/tools
<!-- END ContextStream -->
