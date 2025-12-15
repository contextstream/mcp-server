# ContextStream MCP Server

[![npm version](https://badge.fury.io/js/@contextstream%2Fmcp-server.svg)](https://www.npmjs.com/package/@contextstream/mcp-server)
[![GitHub](https://img.shields.io/github/license/contextstream/mcp-server)](https://github.com/contextstream/mcp-server)
[![MCP Compatible](https://img.shields.io/badge/MCP-Compatible-purple)](https://modelcontextprotocol.io)

> **Persistent memory + semantic search + dependency/impact analysis for Cursor, Claude Code, Windsurf, VS Code, Codex CLI, and any MCP client.**

<div align="center">

## 🧠 Your AI Finally Remembers

**The universal memory layer for AI coding tools.**

One integration. Every AI editor. Persistent memory that never forgets.

[Get Started](https://contextstream.io) · [Documentation](https://contextstream.io/docs/mcp) · [npm Package](https://www.npmjs.com/package/@contextstream/mcp-server)

</div>

> ⭐ **If this saves you time, please star the repo — it helps others find it!**

<div align="center">

<a href="https://contextstream.io">
  <img src="https://customer-vtx4jsqwkbsjpv5b.cloudflarestream.com/f083cfa709a679bd72ef48aca6fe0af2/thumbnails/thumbnail.gif?time=2s&height=600" alt="ContextStream Demo - AI that remembers across sessions" width="600" />
</a>

<sub>Your AI remembers decisions, preferences, and context — across sessions and tools.</sub>

</div>

---

## The Real Cost of AI Amnesia

Every developer using AI tools has felt this frustration:

**Monday morning.** You've spent 3 hours with Claude explaining your authentication architecture, the edge cases, why you chose JWT over sessions, the rate limiting strategy. The AI finally *gets it*. You ship great code together.

**Monday afternoon.** New chat window. The AI has no idea what JWT is in your context. It suggests sessions. You explain everything again.

**Tuesday.** You switch to Cursor for a quick refactor. Start from zero. "We use TypeScript with strict mode." "Our API follows REST conventions." "The user service is in `/src/services`." Again.

**A week later.** "Why did we build it this way?" Nobody remembers. The decision rationale is buried in a closed Slack thread. The AI certainly doesn't know.

**A month later.** New team member joins. Days of onboarding conversations. Explaining the same architectural decisions. Documenting tribal knowledge that should already exist.

**This isn't a minor inconvenience. It's death by a thousand cuts.**

Every re-explanation is lost productivity. Every forgotten decision is technical debt. Every context switch is cognitive load. Every new teammate is weeks of redundant knowledge transfer.

Your AI is brilliant for 30 minutes at a time—then it's a goldfish.

---

## What If Your AI Actually Learned?

**ContextStream gives your AI a permanent brain.**

```
You: "Initialize session. Remember: we use PostgreSQL, TypeScript strict mode,
     and JWT for auth. Rate limits are 100 req/min per user."

...3 weeks later, different tool, new conversation...

You: "What database do we use?"
AI: "You're using PostgreSQL. You also prefer TypeScript with strict mode
     and JWT authentication with 100 req/min rate limiting per user."
```

It remembers. **Across sessions. Across tools. Forever.**

Not just facts—**decisions, context, and reasoning**:

```
You: "Why did we choose PostgreSQL over MongoDB?"
AI: "Based on your captured decision from March 15th: 'Chose PostgreSQL for
     ACID compliance and complex joins in the reporting module. MongoDB
     considered but rejected due to transaction requirements.'"
```

---

## 🚀 2-Minute Setup

### 1. Get Your API Key
Sign up at [contextstream.io](https://contextstream.io) → Settings → API Keys → Create

### 2. Add to Your MCP Config

**Claude Code / Cursor / Windsurf / VS Code:**

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_api_key"
      }
    }
  }
}
```

**Codex CLI** (`~/.codex/config.toml`):
```toml
[mcp_servers.contextstream]
command = "npx"
args = ["-y", "@contextstream/mcp-server"]

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "your_api_key"
```

> Codex expects snake_case `mcp_servers` keys. After editing, fully restart Codex.

> For workspace-pooled rate limiting (Team/Enterprise), the MCP server sends `X-Workspace-Id` based on the active repo/session (or explicit `workspace_id` in tool calls). You can optionally set `CONTEXTSTREAM_WORKSPACE_ID` as a fallback default, but it’s not required and isn’t a good fit if you frequently switch workspaces.

### AI Rules Files (Recommended)

Adding rules files ensures your AI automatically uses ContextStream for memory on every conversation.

> ⚠️ **Critical: Tool Naming Convention**
>
> | AI Tool | Tool Name Format | Example |
> |---------|-----------------|---------|
> | **Claude Code** | `mcp__<server>__<tool>` | `mcp__contextstream__session_init` |
> | **All Others** (Codex, Cursor, Windsurf, Cline, etc.) | `<tool>` (raw) | `session_init` |

**For Claude Code** — Create `CLAUDE.md` in your project root:
```markdown
## ContextStream Integration

| Message | What to Call |
|---------|--------------|
| **1st message** | `mcp__contextstream__session_init(folder_path="...", context_hint="<user's message>")` |
| **2nd+ messages** | `mcp__contextstream__context_smart(user_message="<user's message>")` |
| **After tasks** | `mcp__contextstream__session_capture(...)` |

**NO EXCEPTIONS.** Call these tools before responding.
```

**For Codex CLI** — Create `AGENTS.md` in `~/.codex/` (global) or project root:
```markdown
## ContextStream Integration

| Message | What to Call |
|---------|--------------|
| **1st message** | `session_init(folder_path="...", context_hint="<user's message>")` |
| **2nd+ messages** | `context_smart(user_message="<user's message>")` |
| **After tasks** | `session_capture(...)` |

**NO EXCEPTIONS.** Call these tools before responding.
```

**For other editors** — See [full templates in the docs](https://contextstream.io/docs/mcp).

### 3. Experience Memory

```
You: "Initialize session and remember I prefer functional React components"
```

Open a **new conversation** (even in a different tool):

```
You: "What's my React preference?"
AI: "You prefer functional React components."
```

✨ **That's it. Your AI remembers now.**

---

## Beyond Memory: Intelligence That Compounds

Memory is just the foundation. ContextStream understands your codebase at a deeper level.

### 🔴 Lessons Learned — Never Repeat Mistakes

When your AI makes a mistake—wrong approach, broken build, production issue—capture it as a lesson:

```
You: "Capture lesson: Always run tests before pushing to main"
```

**These lessons surface automatically in future sessions.** Before the AI takes a similar action, it sees the warning. Your AI learns from mistakes just like you do.

| Trigger | Example |
|---------|---------|
| User correction | "No, we use PostgreSQL not MySQL" |
| Production issue | "That deploy broke the API" |
| Workflow mistake | "You forgot to run the linter" |

Lessons are categorized by severity (`critical`, `high`, `medium`, `low`) and automatically retrieved when relevant context is detected.

### 📊 Impact Analysis

```
You: "What breaks if I change the UserService class?"
```

See all dependencies and side effects **before** you refactor. No more surprise breakages.

### 🔍 Semantic Code Search

```
You: "Find where we handle authentication errors"
```

Search by **meaning**, not keywords. Find code by what it does, not what it's named.

### 🧬 Knowledge Graph

Decisions, code, and documentation—all connected. Ask "why" and get answers with full context.

### 🤖 Auto-Context Loading

Context loads **automatically** on first interaction. No manual setup:

```
═══════════════════════════════════════════
🧠 AUTO-CONTEXT LOADED (ContextStream)
═══════════════════════════════════════════
📁 Workspace: acme-corp
📂 Project: backend-api

📋 Recent Decisions:
   • Use PostgreSQL for persistence
   • JWT for authentication

⚠️ Active Lessons:
   • Always run tests before pushing

🧠 Recent Context:
   • [decision] API rate limiting strategy
   • [preference] TypeScript strict mode
═══════════════════════════════════════════
```

---

## 71 MCP Tools

### Session & Memory
| Tool | What It Does |
|------|--------------|
| `session_init` | Initialize with auto-context loading |
| `context_smart` | Get relevant context for any message |
| `session_remember` | Natural language: "Remember X" |
| `session_recall` | Natural language: "What did we decide about X?" |
| `session_capture` | Store decisions, insights, preferences |
| `session_capture_lesson` | **Capture mistakes to prevent repeating them** |
| `session_get_lessons` | Retrieve relevant lessons |

### Code Intelligence
| Tool | What It Does |
|------|--------------|
| `search_semantic` | Find code by meaning |
| `search_hybrid` | Semantic + keyword combined |
| `graph_dependencies` | See what depends on what |
| `graph_impact` | Understand change impact |
| `graph_call_path` | Trace execution flows |
| `graph_unused_code` | Find dead code |

### AI Integration
| Tool | What It Does |
|------|--------------|
| `ai_context` | Build LLM-ready context |
| `ai_context_budget` | Context within token limits |
| `ai_plan` | Generate development plans |
| `ai_tasks` | Break work into tasks |

[**View all 71 tools →**](https://contextstream.io/docs/mcp)

---

## Why Not Built-in Memory?

| Built-in "Memory" | ContextStream |
|-------------------|---------------|
| 🔒 Locked to one vendor | 🌐 **Universal** — works with Cursor, Claude, Windsurf, any MCP client |
| ⏱️ Expires or resets | ♾️ **Persistent** — never lose context |
| 📝 Basic key-value | 🧠 **Semantic** — understands meaning and relationships |
| 👤 Personal only | 👥 **Team-ready** — shared workspace, instant onboarding |
| ❌ No lessons | ✅ **Learns from mistakes** — captures and surfaces lessons |
| ❌ No code understanding | 🔍 **Deep analysis** — dependencies, impact, knowledge graph |
| 🤷 Hope it remembers | 🎯 **Deterministic** — you control what's stored |

---

## Privacy & Security

- **🔐 Encrypted at rest** — AES-256 encryption for all stored data
- **🚫 Never trains on your data** — Your code is yours. Period.
- **🎛️ You control access** — Workspace permissions, API key management
- **🗑️ Delete anytime** — Full data deletion on request

---

## Works Everywhere

ContextStream uses the [Model Context Protocol](https://modelcontextprotocol.io)—the emerging standard for AI tool integrations.

**Supported today:**
- Claude Code
- Cursor
- Windsurf
- VS Code (with MCP extension)
- Codex CLI
- Cline
- Kilo Code
- Roo Code
- Any MCP-compatible client

**One integration. Every tool. Same memory.**

---

## Quick Reference

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `CONTEXTSTREAM_API_URL` | Yes | `https://api.contextstream.io` |
| `CONTEXTSTREAM_API_KEY` | Yes | Your API key |

### Essential Commands

```bash
# Install globally
npm install -g @contextstream/mcp-server

# Or run via npx (recommended for MCP configs)
npx @contextstream/mcp-server
```

### First Session Checklist

1. ✅ Add MCP config to your editor
2. ✅ Start a conversation: "Initialize session for [project-name]"
3. ✅ Tell it your preferences: "Remember we use TypeScript strict mode"
4. ✅ Make a decision: "Capture decision: Using PostgreSQL for the user database"
5. ✅ Open a new conversation and ask: "What are my preferences?"

---

## Links

| Resource | URL |
|----------|-----|
| Website | [contextstream.io](https://contextstream.io) |
| Documentation | [contextstream.io/docs](https://contextstream.io/docs) |
| MCP Setup Guide | [contextstream.io/docs/mcp](https://contextstream.io/docs/mcp) |
| npm Package | [@contextstream/mcp-server](https://www.npmjs.com/package/@contextstream/mcp-server) |
| GitHub | [contextstream/mcp-server](https://github.com/contextstream/mcp-server) |

---

## Contributing

We welcome contributions:

1. **Report bugs** — [Open an issue](https://github.com/contextstream/mcp-server/issues)
2. **Request features** — Share ideas in GitHub Issues
3. **Submit PRs** — Fork, branch, and submit

### Development

```bash
git clone https://github.com/contextstream/mcp-server.git
cd mcp-server
npm install
npm run dev      # Development mode
npm run build    # Production build
npm run typecheck
```

---

## License

MIT

---

<div align="center">

**Stop re-explaining. Start building.**

[Get Started →](https://contextstream.io)

</div>
