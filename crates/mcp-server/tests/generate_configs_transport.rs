//! Regression test for issue #267:
//! `generate-configs --transport=local` must never exit `0` while emitting
//! `"transport": "remote"` JSON on stdout. Without `CONTEXTSTREAM_ALLOW_LOCAL_MCP=1`
//! (or a saved local transport marker), it exits non-zero with empty stdout;
//! with `CONTEXTSTREAM_ALLOW_LOCAL_MCP=1`, it exits `0` and emits local configs.

use serde_json::Value;
use std::process::Command;

#[test]
fn generate_configs_local_transport_fails_closed_without_opt_in_and_emits_local_when_allowed() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 1. Without CONTEXTSTREAM_ALLOW_LOCAL_MCP or saved local marker:
    //    must exit non-zero and write nothing to stdout.
    let denied = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["generate-configs", "--transport=local", "--editors=claude"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CONTEXTSTREAM_ALLOW_LOCAL_MCP")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .output()
        .unwrap();
    let denied_stdout = String::from_utf8_lossy(&denied.stdout);
    let denied_stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        !denied.status.success(),
        "expected non-zero exit when local MCP opt-in is absent, got status {:?}",
        denied.status
    );
    assert!(
        denied_stdout.trim().is_empty(),
        "must not emit remote JSON on stdout when --transport=local is refused:\n{denied_stdout}"
    );
    assert!(
        denied_stderr.contains("CONTEXTSTREAM_ALLOW_LOCAL_MCP=1"),
        "stderr must explain how to opt in:\n{denied_stderr}"
    );

    // 2. With CONTEXTSTREAM_ALLOW_LOCAL_MCP=1:
    //    must exit 0 and emit `"transport": "local"` for Claude Code.
    let allowed = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["generate-configs", "--transport=local", "--editors=claude"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CONTEXTSTREAM_ALLOW_LOCAL_MCP", "1")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "expected exit 0 with CONTEXTSTREAM_ALLOW_LOCAL_MCP=1:\nstderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let parsed: Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert_eq!(parsed["editors"][0]["editor"].as_str(), Some("claude"));
    assert_eq!(parsed["editors"][0]["transport"].as_str(), Some("local"));

    // 3. Default / explicit `--transport=remote` still exits 0 and emits remote config.
    let remote = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["generate-configs", "--transport=remote", "--editors=claude"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("CONTEXTSTREAM_ALLOW_LOCAL_MCP")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .output()
        .unwrap();
    assert!(remote.status.success());
    let remote_parsed: Value = serde_json::from_slice(&remote.stdout).unwrap();
    assert_eq!(
        remote_parsed["editors"][0]["transport"].as_str(),
        Some("remote")
    );
}
