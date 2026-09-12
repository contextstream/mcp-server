//! Exercise the real CLI without reading or changing the operator's credentials.
#![cfg(unix)]

use axum::{http::StatusCode, routing::get, Json, Router};
use serde_json::json;
use std::process::{Command, Stdio};

async fn run_profile_failure(status: StatusCode) -> (String, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/api/v1/auth/me",
        get(move || async move {
            (
                status,
                Json(json!({"data": {
                    "id": "11111111-1111-4111-8111-111111111111",
                    "email": "existing@example.test",
                    "created_at": "2026-01-01T00:00:00Z"
                }})),
            )
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".contextstream");
    std::fs::create_dir(&config_dir).unwrap();
    let credentials_path = config_dir.join("credentials.json");
    let credentials = json!({"api_key": "existing-test-key", "api_url": api_url}).to_string();
    std::fs::write(&credentials_path, &credentials).unwrap();
    let profile_path = home.path().join("profile.json");
    std::fs::write(
        &profile_path,
        json!({
            "device_id": "test-device",
            "user_id": "22222222-2222-4222-8222-222222222222",
            "email": "intended@example.test",
            "api_key": {"id": "test-key-id", "secret": "minted-test-key"},
            "api_url": api_url,
            "profile": {"editors": []}
        })
        .to_string(),
    )
    .unwrap();
    let isolated_home = home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--account-only", "--profile-file"])
            .arg(profile_path)
            .env_clear()
            .env("HOME", &isolated_home)
            .env("CONTEXTSTREAM_API_URL", api_url)
            .env("NO_COLOR", "1")
            .current_dir(isolated_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(credentials_path).unwrap(),
        credentials
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stdout.contains("Signed in as"), "{stdout}");
    assert!(!stdout.contains("Credentials installed"), "{stdout}");
    assert!(!stdout.contains("Credentials replaced"), "{stdout}");
    for secret in ["existing-test-key", "minted-test-key"] {
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
    }
    (stdout, stderr)
}

#[tokio::test]
async fn noninteractive_account_conflict_preserves_credentials_and_explains_recovery() {
    let (_, stderr) = run_profile_failure(StatusCode::OK).await;
    for expected in [
        "existing@example.test",
        "intended@example.test",
        "existing credentials were left untouched",
        "fresh signed-in command",
        "confirm the account switch",
        "already been redeemed",
        "CONTEXTSTREAM_API_KEY / CONTEXTSTREAM_TOKEN",
    ] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
}

#[tokio::test]
async fn failed_credential_verification_does_not_claim_sign_in_success() {
    let (_, stderr) = run_profile_failure(StatusCode::INTERNAL_SERVER_ERROR).await;
    assert!(
        stderr.contains("Could not verify the existing credentials"),
        "{stderr}"
    );
    assert!(stderr.contains("left untouched"), "{stderr}");
}
