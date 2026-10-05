//! Tests for the VCS domain tool.

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use serde_json::json;

/// The request line and JSON body the API receives from one `create_link`
/// call. Any other request the call makes is answered 404.
async fn create_link_request(fields: Value) -> (String, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let (head, body) = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its body");
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_string();
                        let length: usize = head
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse().unwrap())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (head, request[end + 4..end + 4 + length].to_vec());
                        }
                    }
                };
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let is_link = request_line.starts_with("POST /api/v1/integrations/workspaces/")
                    && request_line.contains("/vcs/links");
                let (status, payload) = if is_link {
                    ("200 OK", json!({"id": Uuid::new_v4()}).to_string())
                } else {
                    ("404 Not Found", json!({"error": "not found"}).to_string())
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                if is_link {
                    return (request_line, serde_json::from_slice::<Value>(&body).unwrap());
                }
            }
        })
        .await
        .expect("bounded create_link request")
    });

    let mut config = TestFixtures::test_config();
    config.api_url = format!("http://{address}");
    let tool = VcsTool::new(ContextStreamClient::new(config));
    let mut input = json!({"action": "create_link"});
    for (key, value) in fields.as_object().expect("link fields") {
        input[key] = value.clone();
    }
    tool.execute(input).await.expect("create_link call");
    server.await.expect("create_link server")
}

/// A link whose VCS side is `source_*` and whose ContextStream side is
/// `target_*`, as the tool's schema documents them.
fn link_fields() -> Value {
    json!({
        "source_type": "pull_request",
        "source_id": Uuid::new_v4(),
        "target_type": "task",
        "target_id": Uuid::new_v4(),
    })
}

#[tokio::test]
async fn create_link_sends_the_field_names_the_api_requires() {
    let workspace_id = Uuid::new_v4();
    let mut fields = link_fields();
    fields["workspace_id"] = json!(workspace_id);
    let (request_line, body) = create_link_request(fields.clone()).await;

    assert!(
        request_line.starts_with(&format!(
            "POST /api/v1/integrations/workspaces/{workspace_id}/vcs/links"
        )),
        "unexpected request: {request_line}"
    );
    // The API's request struct is `vcs_object_type`, `vcs_object_id`,
    // `cs_object_type`, `cs_object_id`, all required. Comparing the whole body
    // also proves the tool's own `source_*` and `target_*` names are not sent.
    assert_eq!(
        body,
        json!({
            "vcs_object_type": "pull_request",
            "vcs_object_id": fields["source_id"],
            "cs_object_type": "task",
            "cs_object_id": fields["target_id"],
        })
    );
}

#[tokio::test]
async fn create_link_trims_its_fields_before_sending() {
    let vcs_id = Uuid::new_v4();
    let cs_id = Uuid::new_v4();
    let (_, body) = create_link_request(json!({
        "source_type": "  repo ",
        "source_id": format!(" {vcs_id} "),
        "target_type": " project  ",
        "target_id": format!("\t{cs_id}\n"),
    }))
    .await;
    assert_eq!(body["vcs_object_type"], "repo");
    assert_eq!(body["vcs_object_id"], vcs_id.to_string());
    assert_eq!(body["cs_object_type"], "project");
    assert_eq!(body["cs_object_id"], cs_id.to_string());
}

#[tokio::test]
async fn create_link_names_the_field_that_is_missing_or_wrong() {
    // Validation happens before any request, so no server is needed.
    let tool = VcsTool::new(ContextStreamClient::new(TestFixtures::test_config()));
    let full = link_fields();

    for field in ["source_type", "source_id", "target_type", "target_id"] {
        let mut input = full.clone();
        input["action"] = json!("create_link");
        input.as_object_mut().unwrap().remove(field);
        let error = tool
            .execute(input)
            .await
            .err()
            .unwrap_or_else(|| panic!("a link without {field} must be refused"))
            .to_string();
        assert!(error.contains(field), "{field}: {error}");
    }

    // The API takes both ids as UUIDs. Say so here, instead of passing the
    // value on to be rejected by the server.
    for field in ["source_id", "target_id"] {
        let mut input = full.clone();
        input["action"] = json!("create_link");
        input[field] = json!("not-a-uuid");
        let error = tool
            .execute(input)
            .await
            .err()
            .unwrap_or_else(|| panic!("a non-UUID {field} must be refused"))
            .to_string();
        assert!(
            error.contains(field) && error.contains("UUID"),
            "{field}: {error}"
        );
    }
}
