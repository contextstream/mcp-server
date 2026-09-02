//! Tool metadata and input schema for the `feed` tool.

use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};
use serde_json::Value;

use super::{
    VALID_ACTIONS, VALID_AUDIENCES, VALID_AUTHOR_KINDS, VALID_FEEDBACK_TYPES, VALID_INCLUDES,
    VALID_KINDS, VALID_ORIGINS, VALID_VIEWS,
};
use crate::schema::SchemaBuilder;

pub(super) fn metadata() -> ToolMetadata {
    ToolMetadata {
        name: "feed".to_string(),
        title: "Context Feeds".to_string(),
        description: "Curated workspace, project, and topic activity streams (Context Feeds). \
            Read what changed and why it matters, post agent findings, follow feeds, mark them \
            read, share them across workspaces, tune sources, and trigger curation. Actions: \
            list, ensure (get-or-create the canonical workspace/project feed, or create a topic \
            feed), get, update, archive, items (view=latest|unread|posts|top), post, follow, \
            unfollow, read, share, unshare, feedback, curate, runs, sources, ground (top items \
            for a task). Feeds are curated summaries with citations — distinct from memory \
            (durable notes), coordination (live agent notices), and entity handoffs. When \
            [FEED] lines appear in grounding, open the feed with feed(action=\"items\", \
            feed_id=\"...\") before re-deriving the same context."
            .to_string(),
        category: ToolCategory::Memory,
        annotations: ToolAnnotations::destructive(),
        is_pro: false,
        required_tier: None,
    }
}

pub(super) fn input_schema() -> Value {
    SchemaBuilder::new()
        .description("Read, follow, post to, share, and curate Context Feeds.")
        .string_enum("action", "Action to perform", VALID_ACTIONS, true)
        .uuid(
            "workspace_id",
            "Workspace ID. Defaults to the active session scope.",
            false,
        )
        .uuid(
            "project_id",
            "Project ID. Defaults to the active session scope.",
            false,
        )
        .uuid(
            "feed_id",
            "Feed ID. For items/post/follow/read/curate/runs/get it defaults to the canonical feed of the active scope.",
            false,
        )
        .string_enum(
            "kind",
            "Feed kind for ensure: workspace or project (canonical, get-or-create) or topic (creates a named feed).",
            VALID_KINDS,
            false,
        )
        .string("name", "Feed name (ensure kind=topic, update).", false)
        .string("description", "Feed description (ensure kind=topic, update).", false)
        .object(
            "topic_spec",
            "Topic feed specification (keywords, scopes) for ensure kind=topic / update.",
            false,
        )
        .object(
            "curation_settings",
            "Curation settings override for ensure kind=topic / update.",
            false,
        )
        .string_enum(
            "include",
            "Which feeds to list: owned, shared, or all (default all).",
            VALID_INCLUDES,
            false,
        )
        .boolean("include_archived", "Include archived feeds in list.", false)
        .string_enum(
            "view",
            "Item view for items: latest (default), unread, posts, or top.",
            VALID_VIEWS,
            false,
        )
        .integer("cursor", "Offset cursor from a previous page's next_cursor.", false)
        .integer(
            "limit",
            "Page size (1-100; ground 1-10). Defaults to 20 (ground 5).",
            false,
        )
        .string("since", "RFC 3339 lower bound on item occurred_at (items).", false)
        .uuid("item_id", "Feed item ID (feedback; get with feed_id returns detail).", false)
        .string("title", "Post title (post).", false)
        .string("content", "Post body, markdown allowed (post).", false)
        .array("tags", "Post tags (post).", "string", false)
        .string_enum(
            "author_kind",
            "Who authored the post: agent (default) or human.",
            VALID_AUTHOR_KINDS,
            false,
        )
        .string_enum(
            "feedback_type",
            "Relevance signal for feedback: positive, dismiss, hard_ignore, or not_relevant.",
            VALID_FEEDBACK_TYPES,
            false,
        )
        .boolean("pinned_to_sidebar", "Pin the feed when following.", false)
        .string("muted_until", "RFC 3339 timestamp until which the feed is muted (follow).", false)
        .string("digest_frequency", "Digest cadence when following, e.g. realtime or daily.", false)
        .integer(
            "last_read_sequence",
            "Sequence to mark as read (read). Defaults to the feed's latest sequence.",
            false,
        )
        .uuid("target_workspace_id", "Workspace to share the feed with (share).", false)
        .uuid("target_project_id", "Project to share the feed with (share).", false)
        .string_enum(
            "audience",
            "Share audience: agents (default) or everyone.",
            VALID_AUDIENCES,
            false,
        )
        .uuid("share_id", "Share grant to revoke (unshare).", false)
        .uuid("source_workspace_id", "Source workspace to add (sources).", false)
        .uuid("source_project_id", "Source project to add (sources).", false)
        .string_enum(
            "origin",
            "Source origin when adding: explicit (default) or excluded.",
            VALID_ORIGINS,
            false,
        )
        .string(
            "source_key",
            "Source to remove (sources): \"<workspace_uuid>:<project_uuid or ->\".",
            false,
        )
        .string("query", "Natural-language task anchor for ground.", false)
        .integer(
            "expected_revision",
            "Current feed revision, required for update and archive.",
            false,
        )
        .string(
            "idempotency_key",
            "Idempotency key for writes. Generated when omitted.",
            false,
        )
        .build()
}
