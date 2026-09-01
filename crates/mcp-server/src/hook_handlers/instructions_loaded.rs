//! InstructionsLoaded hook handler.
//!
//! Records rule/instruction file loads so the workspace has a trace of which
//! guidance entered Claude's context during the session.

use anyhow::Result;
use mcp_types::HarnessId;
use serde_json::Value;
use std::fs::{File, Metadata};
use std::io::{Read, Take};
use std::path::Path;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

const MAX_LOADED_RULES_BYTES: u64 = 1024 * 1024;
const CONTEXTSTREAM_START: &str = "<contextstream>";
const CONTEXTSTREAM_END: &str = "</contextstream>";

#[cfg(windows)]
fn metadata_is_unsafe_file(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.is_file()
}

#[cfg(not(windows))]
fn metadata_is_unsafe_file(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink() || !metadata.is_file()
}

fn open_rules_file_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn read_bounded_loaded_rules(path: &Path) -> std::io::Result<String> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "InstructionsLoaded file_path must be absolute",
        ));
    }

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata_is_unsafe_file(&metadata) || metadata.len() > MAX_LOADED_RULES_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "InstructionsLoaded path is not a bounded regular file",
        ));
    }

    let mut file = open_rules_file_no_follow(path)?;
    let opened_metadata = file.metadata()?;
    if metadata_is_unsafe_file(&opened_metadata) || opened_metadata.len() > MAX_LOADED_RULES_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "InstructionsLoaded file changed while it was being validated",
        ));
    }

    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    let mut bounded: Take<&mut File> = Read::by_ref(&mut file).take(MAX_LOADED_RULES_BYTES + 1);
    bounded.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOADED_RULES_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "InstructionsLoaded file exceeds the readiness evidence limit",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "InstructionsLoaded rules are not valid UTF-8",
        )
    })
}

fn valid_rules_hash(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn managed_block_hash(content: &str) -> Option<String> {
    let mut remaining = content;
    while let Some(start) = remaining.find(CONTEXTSTREAM_START) {
        let after_start = &remaining[start..];
        let relative_end = after_start.find(CONTEXTSTREAM_END)?;
        let end = relative_end + CONTEXTSTREAM_END.len();
        let block = &after_start[..end];
        if let Some(hash) = mcp_types::rules_hash::extract_hash_marker(block) {
            if valid_rules_hash(&hash) {
                return Some(hash);
            }
        }
        remaining = &after_start[end..];
    }
    None
}

fn loaded_contextstream_rules_hash(input: &Value) -> std::io::Result<Option<String>> {
    if input.get("hook_event_name").and_then(Value::as_str) != Some("InstructionsLoaded") {
        return Ok(None);
    }
    let Some(file_path) = input.get("file_path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let content = read_bounded_loaded_rules(Path::new(file_path))?;
    Ok(managed_block_hash(&content))
}

fn exact_instruction_load_harness(input: &Value) -> Option<HarnessId> {
    match super::compliance::exact_hook_harness(Some(input)) {
        Some(HarnessId::ClaudeCode) => Some(HarnessId::ClaudeCode),
        _ => None,
    }
}

fn record_loaded_evidence(input: &Value) {
    if !crate::hook_readiness_evidence_enabled() || !crate::managed_hook_invocation() {
        return;
    }
    let Some(harness_id) = exact_instruction_load_harness(input) else {
        tracing::debug!("InstructionsLoaded event had missing or conflicting Claude Code identity");
        return;
    };
    match loaded_contextstream_rules_hash(input) {
        Ok(Some(rules_hash)) => {
            if let Err(error) = mcp_client::harness_readiness::record_direct_instruction_load(
                harness_id,
                &rules_hash,
            ) {
                tracing::warn!(
                    error = %error,
                    "InstructionsLoaded evidence could not be recorded"
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(
                error = %error,
                "InstructionsLoaded event was not eligible for readiness evidence"
            );
        }
    }
}

/// Handle the InstructionsLoaded hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    record_loaded_evidence(&input);
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    if config.is_configured() {
        let reason = input
            .get("load_reason")
            .or_else(|| input.get("reason"))
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");

        let content = serde_json::json!({
            "reason": reason,
            "file_path": input
                .get("file_path")
                .or_else(|| input.get("instruction_path"))
                .cloned(),
            "matched_path": input.get("matched_path").cloned(),
            "captured_at": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(
            &config,
            &format!("Instructions loaded: {}", reason),
            content,
            &["hook", "instructions_loaded", reason],
        )
        .await;
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &Path) -> Value {
        serde_json::json!({
            "hook_event_name": "InstructionsLoaded",
            "file_path": path,
            "load_reason": "session_start"
        })
    }

    #[test]
    fn exact_loaded_file_with_managed_block_yields_bounded_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("CLAUDE.md");
        std::fs::write(
            &path,
            "user prefix\n<contextstream>\n<!-- contextstream-rules-hash: 0123456789abcdef -->\nmanaged\n</contextstream>\nuser suffix\n",
        )
        .expect("rules");

        assert_eq!(
            loaded_contextstream_rules_hash(&input(&path)).expect("read"),
            Some("0123456789abcdef".to_string())
        );
    }

    #[test]
    fn ambiguous_or_forged_inputs_do_not_prove_a_load() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("CLAUDE.md");
        std::fs::write(
            &path,
            "<!-- contextstream-rules-hash: 0123456789abcdef -->\n<contextstream>\nno marker here\n</contextstream>\n",
        )
        .expect("rules");

        assert_eq!(
            loaded_contextstream_rules_hash(&input(&path)).expect("read"),
            None,
            "a marker outside the managed block is not ownership evidence"
        );
        assert_eq!(
            loaded_contextstream_rules_hash(&serde_json::json!({
                "hook_event_name": "PostToolUse",
                "file_path": path
            }))
            .expect("read"),
            None,
            "another hook event cannot masquerade as InstructionsLoaded"
        );
        assert!(loaded_contextstream_rules_hash(&serde_json::json!({
            "hook_event_name": "InstructionsLoaded",
            "file_path": "CLAUDE.md"
        }))
        .is_err());
    }

    #[test]
    fn conflicting_client_identity_cannot_claim_a_claude_instruction_load() {
        assert_eq!(
            exact_instruction_load_harness(&serde_json::json!({
                "hook_event_name": "InstructionsLoaded",
                "client_name": "claude-code"
            })),
            Some(HarnessId::ClaudeCode)
        );
        assert_eq!(
            exact_instruction_load_harness(&serde_json::json!({
                "hook_event_name": "InstructionsLoaded",
                "client_name": "cursor"
            })),
            None,
            "the Claude hook event name must not override a conflicting exact client id"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_loaded_rules_are_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target.md");
        let link = temp.path().join("CLAUDE.md");
        std::fs::write(
            &target,
            "<contextstream>\n<!-- contextstream-rules-hash: 0123456789abcdef -->\n</contextstream>\n",
        )
        .expect("target");
        symlink(&target, &link).expect("symlink");

        assert!(loaded_contextstream_rules_hash(&input(&link)).is_err());
    }
}
