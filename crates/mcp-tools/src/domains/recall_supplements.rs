//! Admit supplemental sources only through fresh primary authorization.
use mcp_client::ContextStreamClient;
use mcp_types::{Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};
use uuid::Uuid;

fn invalid() -> Error {
    Error::Validation("Supplemental recall evidence is unavailable or malformed".into())
}

fn source_ids(decisions: &[Value], docs: &[Value]) -> Result<Vec<Value>> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for (kind, item) in docs.iter().map(|item| (Some("doc"), item)).chain(
        decisions
            .iter()
            .map(|item| (item.get("source").and_then(Value::as_str), item)),
    ) {
        let kind = kind.ok_or_else(invalid)?.to_ascii_lowercase();
        if !matches!(kind.as_str(), "doc" | "event" | "node") {
            return Err(invalid());
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(invalid)?;
        if seen.insert((kind.clone(), id)) {
            sources.push(json!({"kind":kind,"id":id}));
        }
    }
    if sources.len() > 20 {
        return Err(invalid());
    }
    Ok(sources)
}

fn checked_results(
    value: Value,
    workspace: Uuid,
    project: Option<Uuid>,
    query: &str,
    sources: &[Value],
) -> Result<Vec<Value>> {
    if value["evidence_contract"] != "supplemental_primary_v1"
        || value["query"] != query
        || value["workspace_id"] != json!(workspace)
        || value.get("project_id") != Some(&json!(project))
        || value.get("degraded") != Some(&Value::Bool(false))
        || !value
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        return Err(invalid());
    }
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let mut seen = HashSet::new();
    for result in results {
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let kind = result
            .get("source_kind")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if !seen.insert((kind, id)) || !sources.iter().any(|s| s["kind"] == kind && s["id"] == id) {
            return Err(invalid());
        }
        let metadata = &result["metadata"];
        if metadata["source_kind"] != kind || metadata["source_entity_id"] != id {
            return Err(invalid());
        }
        // The selector separately verifies query digest, exact source scope,
        // coverage and evidence version. Never promote a raw list response.
        if metadata["retrieval_provenance"]["evidence_source"] != "primary_authorized_display" {
            return Err(invalid());
        }
        let provenance = &metadata["retrieval_provenance"];
        let source_scope = &provenance["source_scope"];
        let actual_project = source_scope
            .get("project_id")
            .and_then(|v| serde_json::from_value::<Option<Uuid>>(v.clone()).ok())
            .ok_or_else(invalid)?;
        let identity_field = match kind {
            "doc" => "doc_id",
            "event" => "event_id",
            "node" => "node_id",
            _ => return Err(invalid()),
        };
        if metadata[identity_field] != id
            || metadata["source_scope"] != *source_scope
            || source_scope["workspace_id"] != json!(workspace)
            || project
                .zip(actual_project)
                .is_some_and(|(requested, actual)| requested != actual)
            || provenance["version"] != 2
            || provenance["query_sha256"] != format!("{:x}", Sha256::digest(query.as_bytes()))
            || provenance["score_kind"] != "uncalibrated"
            || provenance["calibration"] != "uncalibrated"
        {
            return Err(invalid());
        }
    }
    Ok(results.clone())
}

pub(super) fn mark_unavailable(recall: &mut Value) {
    if let Some(object) = recall.as_object_mut() {
        object.insert("supplemental_results".into(), json!([]));
        object.insert("supplemental_source_ids".into(), json!([]));
        object.insert("supplemental_status".into(), json!("unavailable"));
        object.insert("degraded".into(), json!(true));
        let errors = object.entry("errors").or_insert_with(|| json!([]));
        if let Some(errors) = errors.as_array_mut() {
            errors.push(json!("supplemental_recall_unavailable"));
        }
    }
}

/// Legacy formatting may keep its shape, but not pre-authorization bytes.
/// Do not copy current_truth or conflict claims from a stale discovery row.
pub(super) fn display_views(recall: &Value) -> (Vec<Value>, Vec<Value>) {
    let mut decisions = Vec::new();
    let mut docs = Vec::new();
    if let Some(results) = recall.get("supplemental_results").and_then(Value::as_array) {
        for result in results {
            let Some(mut display) = result.get("metadata").and_then(Value::as_object).cloned()
            else {
                continue;
            };
            display.insert("id".into(), result["id"].clone());
            display.insert(
                "project_id".into(),
                display
                    .get("source_project_id")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            if result["source_kind"] == "doc" {
                docs.push(Value::Object(display));
            } else {
                display.insert("source".into(), result["source_kind"].clone());
                if !display.contains_key("summary") {
                    display.insert(
                        "summary".into(),
                        display.get("title").cloned().unwrap_or(Value::Null),
                    );
                }
                if !display.contains_key("details") {
                    display.insert(
                        "details".into(),
                        display
                            .get("content_preview")
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
                decisions.push(Value::Object(display));
            }
        }
    }
    (decisions, docs)
}

pub(super) async fn attach(
    client: &ContextStreamClient,
    workspace: Option<Uuid>,
    project: Option<Uuid>,
    query: &str,
    decisions: &[Value],
    docs: &[Value],
    recall: &mut Value,
) {
    let attempt =
        async {
            let workspace = workspace.ok_or_else(invalid)?;
            let sources = source_ids(decisions, docs)?;
            if sources.is_empty() {
                return Ok((Vec::new(), sources));
            }
            // Never cache this response: docs do not carry memory cache revisions.
            let value: Value = client.post("/session/recall/evidence", json!({
            "query":query,"workspace_id":workspace,"project_id":project,"sources":sources
        })).await?;
            checked_results(value, workspace, project, query, &sources)
                .map(|results| (results, sources))
        };
    match tokio::time::timeout(Duration::from_millis(1800), attempt).await {
        Ok(Ok((results, sources))) => {
            if let Some(object) = recall.as_object_mut() {
                object.insert("supplemental_results".into(), json!(results));
                object.insert("supplemental_source_ids".into(), json!(sources));
                object.insert("supplemental_status".into(), json!("available"));
            }
        }
        _ => mark_unavailable(recall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inputs_are_only_typed_ids_and_collisions_are_not_deduplicated() {
        let id = Uuid::new_v4();
        let decisions = vec![
            json!({"id":id,"source":"event","summary":"untrusted"}),
            json!({"id":id,"source":"node"}),
        ];
        let docs = vec![json!({"id":id,"content":"untrusted"}), json!({"id":id})];
        let sources = source_ids(&decisions, &docs).unwrap();
        assert_eq!(sources.len(), 3);
        assert!(sources.iter().all(|s| s.as_object().unwrap().len() == 2));
        assert!(source_ids(&[json!({"id":id})], &[]).is_err());
    }
    #[test]
    fn old_or_malformed_server_responses_cannot_become_evidence() {
        assert!(
            checked_results(json!({"results":[]}), Uuid::nil(), None, "graphics", &[]).is_err()
        );
        let mut recall = json!({"results":[],"degraded":false,"errors":[]});
        mark_unavailable(&mut recall);
        assert_eq!(recall["degraded"], true);
        assert_eq!(recall["supplemental_status"], "unavailable");
    }

    fn response(workspace: Uuid, id: Uuid, content: &str) -> Value {
        let scope = json!({"workspace_id":workspace,"project_id":null});
        json!({"evidence_contract":"supplemental_primary_v1", "workspace_id":workspace,
            "project_id":null,"query":"graphics","degraded":false,"errors":[], "results":[{
                "id":id,"source_kind":"doc","result_type":"doc","score":0.0,
                "metadata":{"doc_id":id,"source_kind":"doc","source_entity_id":id,
                    "title":"graphics","content_preview":content,"source_scope":scope,"source_project_id":null,
                    "retrieval_provenance":{"version":2,"evidence_source":"primary_authorized_display",
                        "score_kind":"uncalibrated","calibration":"uncalibrated","source_scope":scope,
                        "query_sha256":format!("{:x}",Sha256::digest(b"graphics")),
                        "query_term_matches":1,"query_term_count":1,"lexical_query_coverage":1.0}}}]})
    }

    #[test]
    fn responses_require_exact_query_identity_and_actual_source_scope() {
        let ws = Uuid::new_v4();
        let id = Uuid::new_v4();
        let sources = vec![json!({"kind":"doc","id":id})];
        let valid = response(ws, id, "primary");
        assert_eq!(
            checked_results(valid.clone(), ws, None, "graphics", &sources)
                .unwrap()
                .len(),
            1
        );
        for (path, replacement) in [
            ("/query", json!("different")),
            ("/results/0/metadata/doc_id", json!(Uuid::new_v4())),
            (
                "/results/0/metadata/retrieval_provenance/query_sha256",
                json!("0".repeat(64)),
            ),
            (
                "/results/0/metadata/retrieval_provenance/source_scope",
                json!({"workspace_id":ws}),
            ),
            (
                "/results/0/metadata/source_scope/workspace_id",
                json!(Uuid::new_v4()),
            ),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(path).unwrap() = replacement;
            assert!(
                checked_results(invalid, ws, None, "graphics", &sources).is_err(),
                "accepted {path}"
            );
        }
    }

    #[tokio::test]
    async fn repeated_calls_reauthorize_and_never_replay_revoked_document_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let ws = Uuid::new_v4();
        let id = Uuid::new_v4();
        let server = tokio::spawn(async move {
            for turn in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 32768);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        assert!(headers.starts_with("POST /api/v1/session/recall/evidence "));
                        let length: usize = headers
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .unwrap()
                            .1
                            .trim()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            let request: Value =
                                serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
                            assert_eq!(request["sources"], json!([{"kind":"doc","id":id}]));
                            assert_eq!(request.as_object().unwrap().len(), 4);
                            break;
                        }
                    }
                }
                let mut payload = response(ws, id, &format!("primary version {turn}"));
                if turn == 2 {
                    payload["results"] = json!([]);
                }
                let payload = payload.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",payload.len(),payload).as_bytes()).await.unwrap();
            }
        });
        let mut config = crate::testing::TestFixtures::test_config();
        config.api_url = format!("http://{address}");
        let client = ContextStreamClient::new(config);
        for turn in 0..3 {
            let mut recall = json!({"results":[]});
            attach(
                &client,
                Some(ws),
                None,
                "graphics",
                &[],
                &[json!({"id":id,"content":"stale discovery bytes"})],
                &mut recall,
            )
            .await;
            assert_eq!(recall["supplemental_status"], "available");
            let (_, docs) = display_views(&recall);
            if turn == 2 {
                assert!(docs.is_empty());
            } else {
                assert_eq!(
                    docs[0]["content_preview"],
                    format!("primary version {turn}")
                );
            }
        }
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
