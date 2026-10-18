//! Patch operations for `memory(action="update_doc", ops=[...])`.
//!
//! An agent that edits a doc used to resend the whole document. The model
//! generates every byte of a tool call, so a 12 KB doc cost it about 35 s of
//! output to change one paragraph. With `ops` it sends only the change; the
//! MCP server reads the current content, applies the operations in order and
//! writes the full result with the existing update call.

use mcp_types::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One edit to a markdown document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DocPatchOp {
    /// Add `content` after the end of the document.
    Append { content: String },
    /// Add `content` before the start of the document.
    Prepend { content: String },
    /// Replace the body of the section whose heading text is `heading`
    /// (everything after the heading line, up to the next heading of the
    /// same or a higher level). The heading line itself is kept.
    ReplaceSection { heading: String, content: String },
    /// Replace the only occurrence of `old` with `new`.
    StrReplace { old: String, new: String },
}

/// JSON schema for the `ops` parameter.
pub fn doc_ops_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "description": "Edits to apply to the current doc content, in order, instead of resending the whole document. Prefer these for any change to an existing doc. Mutually exclusive with content. Each item is one of: {\"op\":\"append\",\"content\":\"...\"}, {\"op\":\"prepend\",\"content\":\"...\"}, {\"op\":\"replace_section\",\"heading\":\"Rollout\",\"content\":\"new section body\"} (replaces the body under that markdown heading up to the next heading of the same or higher level; the heading line is kept), {\"op\":\"str_replace\",\"old\":\"exact text\",\"new\":\"replacement\"} (old must occur exactly once).",
        "items": {
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["append", "prepend", "replace_section", "str_replace"]
                },
                "content": { "type": "string", "description": "Text for append, prepend or replace_section" },
                "heading": { "type": "string", "description": "Heading text of the section to replace (replace_section)" },
                "old": { "type": "string", "description": "Exact text to replace; must occur once (str_replace)" },
                "new": { "type": "string", "description": "Replacement text (str_replace)" }
            },
            "required": ["op"]
        }
    })
}

/// Apply `ops` to `content` in order.
pub fn apply_doc_ops(content: &str, ops: &[DocPatchOp]) -> Result<String> {
    if ops.is_empty() {
        return Err(Error::Validation(
            "ops is empty; pass at least one operation".to_string(),
        ));
    }
    let mut doc = content.to_string();
    for (index, op) in ops.iter().enumerate() {
        doc = apply_one(&doc, op).map_err(|message| {
            Error::Validation(format!("ops[{index}] ({}): {message}", op_name(op)))
        })?;
    }
    Ok(doc)
}

fn op_name(op: &DocPatchOp) -> &'static str {
    match op {
        DocPatchOp::Append { .. } => "append",
        DocPatchOp::Prepend { .. } => "prepend",
        DocPatchOp::ReplaceSection { .. } => "replace_section",
        DocPatchOp::StrReplace { .. } => "str_replace",
    }
}

fn apply_one(doc: &str, op: &DocPatchOp) -> std::result::Result<String, String> {
    match op {
        DocPatchOp::Append { content } => {
            let addition = content.trim_matches('\n');
            if doc.trim().is_empty() {
                return Ok(format!("{addition}\n"));
            }
            Ok(format!("{}\n\n{addition}\n", doc.trim_end_matches('\n')))
        }
        DocPatchOp::Prepend { content } => {
            let addition = content.trim_matches('\n');
            if doc.trim().is_empty() {
                return Ok(format!("{addition}\n"));
            }
            Ok(format!("{addition}\n\n{}", doc.trim_start_matches('\n')))
        }
        DocPatchOp::StrReplace { old, new } => {
            if old.is_empty() {
                return Err("old must not be empty".to_string());
            }
            match doc.matches(old.as_str()).count() {
                0 => Err("old text was not found in the doc".to_string()),
                1 => Ok(doc.replacen(old.as_str(), new, 1)),
                n => Err(format!(
                    "old text occurs {n} times; include more surrounding text so it matches once"
                )),
            }
        }
        DocPatchOp::ReplaceSection { heading, content } => replace_section(doc, heading, content),
    }
}

/// An ATX heading line: its level and its text.
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    // More than three spaces of indentation is a code block, not a heading.
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    Some((level, normalize_heading_text(text)))
}

fn normalize_heading_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_fence(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Headings of `doc` as (line index, level, text), skipping fenced code.
fn headings(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut found = Vec::new();
    let mut fence: Option<&'static str> = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(marker) = is_fence(line) {
            match fence {
                Some(open) if open == marker => fence = None,
                None => fence = Some(marker),
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some((level, text)) = parse_heading(line) {
            found.push((index, level, text));
        }
    }
    found
}

fn replace_section(doc: &str, heading: &str, content: &str) -> std::result::Result<String, String> {
    // The heading may be given with its markdown hashes ("## Rollout").
    let (wanted_level, wanted_text) = match parse_heading(heading.trim()) {
        Some((level, text)) => (Some(level), text),
        None => (None, normalize_heading_text(heading)),
    };
    if wanted_text.is_empty() {
        return Err("heading must not be empty".to_string());
    }
    let lines: Vec<&str> = doc.split('\n').collect();
    let all = headings(&lines);
    let level_ok = |level: usize| wanted_level.is_none_or(|wanted| wanted == level);
    let mut matches: Vec<&(usize, usize, String)> = all
        .iter()
        .filter(|(_, level, text)| level_ok(*level) && *text == wanted_text)
        .collect();
    if matches.is_empty() {
        let lower = wanted_text.to_lowercase();
        matches = all
            .iter()
            .filter(|(_, level, text)| level_ok(*level) && text.to_lowercase() == lower)
            .collect();
    }
    let (start, level) = match matches.as_slice() {
        [] => {
            let available: Vec<&str> = all.iter().map(|(_, _, text)| text.as_str()).collect();
            return Err(if available.is_empty() {
                format!("no heading \"{wanted_text}\"; the doc has no markdown headings")
            } else {
                format!(
                    "no heading \"{wanted_text}\"; headings are: {}",
                    available.join(" | ")
                )
            });
        }
        [(start, level, _)] => (*start, *level),
        many => {
            return Err(format!(
                "heading \"{wanted_text}\" matches {} sections; pass it with its level (e.g. \"## {wanted_text}\") or use str_replace",
                many.len()
            ))
        }
    };
    let end = all
        .iter()
        .find(|(index, other_level, _)| *index > start && *other_level <= level)
        .map(|(index, _, _)| *index)
        .unwrap_or(lines.len());

    let body = content.trim_matches('\n');
    let mut out: Vec<String> = lines[..=start]
        .iter()
        .map(|line| line.to_string())
        .collect();
    out.push(String::new());
    if !body.is_empty() {
        out.push(body.to_string());
    }
    if end < lines.len() {
        out.push(String::new());
        out.extend(lines[end..].iter().map(|line| line.to_string()));
        Ok(out.join("\n"))
    } else {
        Ok(format!("{}\n", out.join("\n").trim_end_matches('\n')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Title\n\nIntro.\n\n## Plan\n\nOld plan.\n\n### Detail\n\nOld detail.\n\n## Rollout\n\nOld rollout.\n";

    fn section(heading: &str, content: &str) -> DocPatchOp {
        DocPatchOp::ReplaceSection {
            heading: heading.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn replace_section_spans_nested_headings_up_to_the_next_peer() {
        let out = apply_doc_ops(DOC, &[section("Plan", "New plan.")]).unwrap();
        assert_eq!(
            out,
            "# Title\n\nIntro.\n\n## Plan\n\nNew plan.\n\n## Rollout\n\nOld rollout.\n"
        );
    }

    #[test]
    fn replace_section_of_a_nested_heading_stops_at_the_parent_peer() {
        let out = apply_doc_ops(DOC, &[section("### Detail", "New detail.")]).unwrap();
        assert!(out.contains("### Detail\n\nNew detail.\n\n## Rollout"));
        assert!(out.contains("Old plan."));
    }

    #[test]
    fn replace_section_last_section_ends_the_doc() {
        let out = apply_doc_ops(DOC, &[section("rollout", "Now.\n")]).unwrap();
        assert!(out.ends_with("## Rollout\n\nNow.\n"));
    }

    #[test]
    fn replace_section_reports_missing_and_ambiguous_headings() {
        let err = apply_doc_ops(DOC, &[section("Nope", "x")]).unwrap_err();
        assert!(err.to_string().contains("no heading \"Nope\""));
        assert!(err.to_string().contains("Rollout"));

        let doubled = "## Notes\n\na\n\n# Other\n\n## Notes\n\nb\n";
        let err = apply_doc_ops(doubled, &[section("Notes", "x")]).unwrap_err();
        assert!(err.to_string().contains("matches 2 sections"));
    }

    #[test]
    fn headings_inside_code_fences_are_ignored() {
        let doc = "## Real\n\n```md\n## Fake\n```\n\nbody\n";
        let out = apply_doc_ops(doc, &[section("Real", "new")]).unwrap();
        assert_eq!(out, "## Real\n\nnew\n");
        assert!(apply_doc_ops(doc, &[section("Fake", "x")]).is_err());
    }

    #[test]
    fn str_replace_requires_exactly_one_match() {
        let out = apply_doc_ops(
            DOC,
            &[DocPatchOp::StrReplace {
                old: "Old rollout.".into(),
                new: "New rollout.".into(),
            }],
        )
        .unwrap();
        assert!(out.contains("New rollout."));

        let zero = apply_doc_ops(
            DOC,
            &[DocPatchOp::StrReplace {
                old: "absent".into(),
                new: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(zero.to_string().contains("not found"));

        let many = apply_doc_ops(
            DOC,
            &[DocPatchOp::StrReplace {
                old: "Old".into(),
                new: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(many.to_string().contains("occurs 3 times"));
    }

    #[test]
    fn append_and_prepend_keep_one_blank_line_between_blocks() {
        let ops = [
            DocPatchOp::Append {
                content: "## Log\n\n- entry\n".into(),
            },
            DocPatchOp::Prepend {
                content: "> status: draft".into(),
            },
        ];
        let out = apply_doc_ops("# T\n\nbody\n", &ops).unwrap();
        assert_eq!(out, "> status: draft\n\n# T\n\nbody\n\n## Log\n\n- entry\n");
        assert_eq!(
            apply_doc_ops(
                "",
                &[DocPatchOp::Append {
                    content: "x".into()
                }]
            )
            .unwrap(),
            "x\n"
        );
    }

    #[test]
    fn ops_apply_in_order_and_errors_name_the_failing_op() {
        let ops = [
            DocPatchOp::StrReplace {
                old: "Intro.".into(),
                new: "Intro v2.".into(),
            },
            section("Missing", "x"),
        ];
        let err = apply_doc_ops(DOC, &ops).unwrap_err().to_string();
        assert!(err.contains("ops[1] (replace_section)"), "{err}");
        assert!(apply_doc_ops(DOC, &[]).is_err());
    }

    #[test]
    fn ops_deserialize_from_the_tool_shape() {
        let ops: Vec<DocPatchOp> = serde_json::from_value(serde_json::json!([
            {"op": "append", "content": "a"},
            {"op": "replace_section", "heading": "Plan", "content": "b"},
            {"op": "str_replace", "old": "c", "new": "d"},
        ]))
        .unwrap();
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[1], DocPatchOp::ReplaceSection { .. }));
    }
}
