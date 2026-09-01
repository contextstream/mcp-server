//! Shared drift handling for read-side MCP tool actions.
//!
//! When an AI agent calls a quiet read-only MCP tool against a session
//! whose workspace_id was bound earlier but whose current auth
//! credentials no longer grant access (token rotated, account
//! switched, stale cached workspace_id), the API returns 403 / 401.
//! Bubbling that raw "Forbidden: No access to workspace" error to the
//! agent makes them think they did something wrong — they didn't, the
//! session binding just drifted.
//!
//! This module is the single source of truth for the conversion.
//! Read-action handlers pattern-match on the error and call into here:
//!
//! ```ignore
//! match self.client.list_tasks(...).await {
//!     Ok(rows) => Ok(format_rows(rows)),
//!     Err(err) if workspace_drift::is_workspace_access_error(&err) => {
//!         return Ok(workspace_drift::drift_collection_result(
//!             "tasks", workspace_id,
//!         ));
//!     }
//!     Err(err) => Err(err),
//! }
//! ```
//!
//! Writes (`create_*`, `update_*`, `delete_*`, `capture_*`,
//! `submit_*`, etc.) MUST keep bubbling 403/401 — they're explicit
//! user actions where a forbidden response is meaningful, and
//! silencing them would hide real bugs.
//!
//! The first home of this logic was `flash.rs` (v0.2.98), where it
//! still works exactly the same way — `flash` now re-exports these
//! helpers so there's one source of truth.

use mcp_types::tool::ToolResult;
use mcp_types::{Error, ErrorCode};
use serde_json::Value;
use uuid::Uuid;

/// Match the two HTTP codes that mean "the session is bound to a
/// workspace the current credentials can't access":
///
/// - **403 Forbidden** — the canonical case. Token is valid, user has
///   an account, but the workspace_id on the request isn't one of
///   their own and they aren't a member.
/// - **401 Unauthorized** — token expired / missing entirely. Same
///   user-visible symptom (the agent gets a forbidden-style response
///   on a quiet read), so we bucket it together for read actions and
///   surface the same re-init hint.
///
/// Other HTTP codes are intentionally NOT considered drift:
///
/// - 404 / 422 are already handled per-action as "missing" / "empty"
///   results (`is_recoverable_read_error` covers those).
/// - 5xx / 429 / etc. are real errors and should bubble.
pub fn is_workspace_access_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Http {
            code: ErrorCode::Forbidden,
            ..
        } | Error::Http {
            code: ErrorCode::Unauthorized,
            ..
        }
    )
}

/// True when an instruction-cache read can be safely converted into an
/// empty result — 404 / 422 only. Lifted from `flash.rs`.
pub fn is_recoverable_read_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Http {
            code: ErrorCode::NotFound,
            ..
        } | Error::Http {
            code: ErrorCode::ValidationError,
            ..
        }
    )
}

/// The standard re-init hint embedded in every drift result. Kept
/// canonical so dashboards can grep for the exact phrase if they want
/// to flag drift events visually.
pub fn drift_text_hint(workspace_id: Option<Uuid>) -> String {
    let workspace_phrase = workspace_id
        .map(|id| format!("workspace {} ", id))
        .unwrap_or_else(|| "the bound workspace ".to_string());
    format!(
        "Session is bound to {}which the current credentials can't access. \
         Re-run init(folder_path=\"...\") to rebind this session.",
        workspace_phrase
    )
}

/// Generic builder for "I would have read X but the workspace drifted"
/// responses. Action handlers pass:
///
/// - `kind`: the natural collective noun for what was being asked
///   (`"tasks"`, `"lessons"`, `"plans"`, `"decisions"`, …). Lands in
///   the lead text and as a `kind` field on the structured payload so
///   downstream callers can branch on it.
/// - `workspace_id`: the bound workspace, when known. Embedded into
///   the hint string so operators can identify which workspace
///   drifted from log lines.
/// - `extras`: any extra structured fields the action wants to carry
///   into the empty payload (e.g. `{"plan_id": "..."}`). Merged into
///   the response object; `kind` / `scope_status` / `hint` always
///   take precedence over caller-supplied keys.
pub fn drift_collection_result(
    kind: &str,
    workspace_id: Option<Uuid>,
    extras: Option<Value>,
) -> ToolResult {
    let hint = drift_text_hint(workspace_id);
    let text = format!("Loaded 0 {} (workspace access drift). {}", kind, hint);
    let mut payload = serde_json::Map::new();
    if let Some(Value::Object(extra_map)) = extras {
        for (k, v) in extra_map {
            payload.insert(k, v);
        }
    }
    payload.insert("items".to_string(), Value::Array(Vec::new()));
    payload.insert("kind".to_string(), Value::String(kind.to_string()));
    payload.insert(
        "scope_status".to_string(),
        Value::String("drift".to_string()),
    );
    if let Some(ws) = workspace_id {
        payload.insert("workspace_id".to_string(), Value::String(ws.to_string()));
    } else {
        payload.insert("workspace_id".to_string(), Value::Null);
    }
    payload.insert("hint".to_string(), Value::String(hint));
    ToolResult::with_structured(text, Value::Object(payload))
}

/// Singular version for `get_*` actions that fetch one resource by id
/// rather than a list. Same shape as `drift_collection_result` but the
/// payload's collective container is `null` instead of `[]` and the
/// lead text says "Could not load X".
pub fn drift_single_result(
    kind: &str,
    workspace_id: Option<Uuid>,
    extras: Option<Value>,
) -> ToolResult {
    let hint = drift_text_hint(workspace_id);
    let text = format!("Could not load {} (workspace access drift). {}", kind, hint);
    let mut payload = serde_json::Map::new();
    if let Some(Value::Object(extra_map)) = extras {
        for (k, v) in extra_map {
            payload.insert(k, v);
        }
    }
    payload.insert("item".to_string(), Value::Null);
    payload.insert("kind".to_string(), Value::String(kind.to_string()));
    payload.insert(
        "scope_status".to_string(),
        Value::String("drift".to_string()),
    );
    if let Some(ws) = workspace_id {
        payload.insert("workspace_id".to_string(), Value::String(ws.to_string()));
    } else {
        payload.insert("workspace_id".to_string(), Value::Null);
    }
    payload.insert("hint".to_string(), Value::String(hint));
    ToolResult::with_structured(text, Value::Object(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::tool::ContentItem;

    #[test]
    fn workspace_access_error_matches_403_and_401() {
        assert!(is_workspace_access_error(&Error::http(
            403,
            "Forbidden: No access to workspace"
        )));
        assert!(is_workspace_access_error(&Error::http(401, "Unauthorized")));
    }

    #[test]
    fn workspace_access_error_does_not_swallow_other_codes() {
        assert!(!is_workspace_access_error(&Error::http(404, "missing")));
        assert!(!is_workspace_access_error(&Error::http(422, "validation")));
        assert!(!is_workspace_access_error(&Error::http(500, "boom")));
        assert!(!is_workspace_access_error(&Error::http(429, "slow")));
        assert!(!is_workspace_access_error(&Error::Validation(
            "nope".into()
        )));
    }

    #[test]
    fn recoverable_read_error_matches_404_and_422_only() {
        assert!(is_recoverable_read_error(&Error::http(404, "missing")));
        assert!(is_recoverable_read_error(&Error::http(422, "validation")));
        assert!(!is_recoverable_read_error(&Error::http(403, "forbidden")));
        assert!(!is_recoverable_read_error(&Error::http(401, "no auth")));
        assert!(!is_recoverable_read_error(&Error::http(500, "boom")));
    }

    #[test]
    fn drift_text_hint_uses_workspace_id_when_known() {
        let ws = Uuid::nil();
        let hint = drift_text_hint(Some(ws));
        assert!(hint.contains(&ws.to_string()));
        assert!(hint.contains("Re-run init"));
    }

    #[test]
    fn drift_text_hint_falls_back_when_workspace_unknown() {
        let hint = drift_text_hint(None);
        assert!(hint.contains("the bound workspace"));
        assert!(hint.contains("Re-run init"));
    }

    #[test]
    fn drift_collection_result_carries_drift_status_and_kind() {
        let ws = Uuid::nil();
        let result = drift_collection_result("tasks", Some(ws), None);
        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured present");
        assert_eq!(
            structured.get("scope_status"),
            Some(&Value::String("drift".to_string()))
        );
        assert_eq!(
            structured.get("kind"),
            Some(&Value::String("tasks".to_string()))
        );
        assert_eq!(structured.get("items"), Some(&Value::Array(Vec::new())));
        assert_eq!(
            structured.get("workspace_id"),
            Some(&Value::String(ws.to_string()))
        );
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("workspace access drift"));
                assert!(text.contains("Re-run init"));
                // Raw HTTP markers must NEVER leak to agents.
                assert!(!text.contains("403"));
                assert!(!text.contains("401"));
                assert!(!text.contains("Forbidden"));
                assert!(!text.contains("Unauthorized"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[test]
    fn drift_collection_result_merges_extras_under_canonical_keys() {
        let extras = serde_json::json!({"plan_id": "abc-123", "kind": "should-not-override"});
        let result = drift_collection_result("tasks", None, Some(extras));
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("kind"),
            Some(&Value::String("tasks".to_string())),
            "canonical `kind` must override caller-supplied"
        );
        assert_eq!(
            structured.get("plan_id"),
            Some(&Value::String("abc-123".to_string()))
        );
    }

    #[test]
    fn drift_single_result_uses_null_item_and_could_not_load_phrase() {
        let result = drift_single_result("decision", None, None);
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured.get("item"), Some(&Value::Null));
        assert_eq!(
            structured.get("scope_status"),
            Some(&Value::String("drift".to_string()))
        );
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("Could not load decision"));
                assert!(text.contains("workspace access drift"));
                assert!(!text.contains("403"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[test]
    fn drift_workspace_id_is_null_when_absent() {
        let result = drift_collection_result("plans", None, None);
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured.get("workspace_id"), Some(&Value::Null));
    }
}
