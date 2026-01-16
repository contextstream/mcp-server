# Changelog

## 0.4.34

**Your AI assistant just got better at following instructions.**

This release focuses on making sure your AI actually uses ContextStream when it should—no more watching it grep through files when a single semantic search would do.

### What's New

- **Claude Code Hooks** — Optional hooks that automatically redirect local file searches to ContextStream's semantic search. Your AI gets better results faster, and you save tokens. Install with `npx -y @contextstream/mcp-server setup` or `generate_rules(editors=["claude"])`.

- **Smarter Reminders** — The API now reminds your AI to search ContextStream first, every time. Even if instructions drift during long conversations, the reminders keep it on track.

- **Lessons That Stick** — Made a mistake once? ContextStream surfaces relevant lessons before your AI repeats it. Past corrections now actively prevent future errors.

- **Automatic Update Prompts** — When your rules or MCP server version falls behind, you'll get a clear nudge to update. Updates are safe—your custom rules are preserved.

- **Notion Project Support** — Pages created via the Notion integration now link to your current project for better organization.

### Upgrading

```bash
npm update @contextstream/mcp-server
```

Or re-run setup to get the latest hooks:

```bash
npx -y @contextstream/mcp-server setup
```
