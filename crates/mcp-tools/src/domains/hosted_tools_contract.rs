//! Cross-repo & cross-service consumer contract verification hook for hosted `check` (Phase 3.3).

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use futures::future::join_all;
use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::contract_check::{
    drive_contract_stream, encode_contract_b64, ConsumerContractTarget, ContractCheckRequest,
    ContractCheckResponse, ContractFileInput,
};
use mcp_client::frames::{self, Event};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::{ContentItem, ToolResult};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domains::slipstream::listing_workspace;
use crate::peer;

const MAX_CHANGED_PATHS: usize = 200;
const MAX_CONTRACT_FILES: usize = 24;
const MAX_CONTRACT_FILE_BYTES: usize = 64 * 1024;
const MAX_CONTRACT_TOTAL_BYTES: usize = 512 * 1024;
const MAX_CONSUMERS: usize = 4;
const CONTRACT_CHECK_PLAN_TIMEOUT: Duration = Duration::from_secs(20);
const CONSUMER_RUN_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DIAGNOSTIC_LINES: usize = 25;
const MAX_DIAGNOSTIC_BYTES: usize = 2048;

#[derive(Debug, Clone, Serialize)]
pub struct ConsumerContractOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    pub repo_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub language: String,
    pub command: String,
    pub ran: bool,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused_reason: Option<String>,
    pub errors: Vec<String>,
}

pub fn contract_check_env_enabled() -> bool {
    !mcp_client::command_hints::switched_off(
        std::env::var("CONTEXTSTREAM_CONTRACT_CHECK")
            .ok()
            .as_deref(),
    )
}

pub fn is_contract_candidate_path(path: &str, total_overlay_files: usize) -> bool {
    let lower = path.trim().replace('\\', "/").to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.ends_with(".proto")
        || base.ends_with(".graphql")
        || base.ends_with(".gql")
        || base.ends_with(".avsc")
        || base.ends_with(".sql")
        || base.ends_with(".d.ts")
        || ((base.starts_with("openapi") || base.starts_with("swagger"))
            && (base.ends_with(".json") || base.ends_with(".yaml") || base.ends_with(".yml")))
    {
        return true;
    }
    let padded = format!("/{lower}");
    if [
        "/contracts/",
        "/openapi/",
        "/swagger/",
        "/api/",
        "/routes/",
        "/handlers/",
        "/schema/",
        "/schemas/",
        "/types/",
        "/sdk/",
        "/proto/",
    ]
    .iter()
    .any(|seg| padded.contains(seg))
    {
        return true;
    }
    total_overlay_files <= MAX_CONTRACT_FILES
        && (lower.ends_with(".ts")
            || lower.ends_with(".tsx")
            || lower.ends_with(".rs")
            || lower.ends_with(".go")
            || lower.ends_with(".py"))
}

pub async fn collect_contract_inputs(
    snapshot: &CheckoutSnapshot,
) -> (Vec<String>, Vec<ContractFileInput>) {
    let mut changed_paths = Vec::new();
    let mut seen_paths = HashSet::new();
    for entry in &snapshot.overlay {
        if changed_paths.len() >= MAX_CHANGED_PATHS {
            break;
        }
        if seen_paths.insert(entry.path.clone()) {
            changed_paths.push(entry.path.clone());
        }
    }
    for path in &snapshot.deletes {
        if changed_paths.len() >= MAX_CHANGED_PATHS {
            break;
        }
        if seen_paths.insert(path.clone()) {
            changed_paths.push(path.clone());
        }
    }

    let withheld: HashSet<&str> = snapshot.withheld.iter().map(|w| w.path.as_str()).collect();
    let total_overlay = snapshot.overlay.len();
    let mut contract_files = Vec::new();
    let mut total_bytes = 0usize;

    for entry in &snapshot.overlay {
        if contract_files.len() >= MAX_CONTRACT_FILES {
            break;
        }
        if entry.mode == "120000" || withheld.contains(entry.path.as_str()) {
            continue;
        }
        if !is_contract_candidate_path(&entry.path, total_overlay) {
            continue;
        }
        let file_name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        if ContextStreamClient::is_secret_file_name(file_name)
            || ContextStreamClient::is_secret_file_name(&entry.path)
        {
            continue;
        }
        let remaining = MAX_CONTRACT_TOTAL_BYTES - total_bytes;
        let Some(raw) = snapshot
            .read_overlay_blob(entry, MAX_CONTRACT_FILE_BYTES.min(remaining))
            .await
        else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&raw) else {
            continue;
        };
        if ContextStreamClient::secret_content_reason(text).is_some() {
            continue;
        }
        total_bytes += raw.len();
        let sha256 = hex::encode(Sha256::digest(&raw));
        contract_files.push(ContractFileInput {
            source_path: entry.path.clone(),
            target_path: None,
            content_b64: encode_contract_b64(&raw),
            sha256: Some(sha256),
            mode: Some(entry.mode.clone()),
        });
    }

    (changed_paths, contract_files)
}

pub fn extract_compiler_errors(language: &str, output: &str) -> Vec<String> {
    let ts_re = Regex::new(r"(error\s+TS\d+:|:\s*error\s+TS\d+:)").unwrap();
    let rust_err_re = Regex::new(r"^error(\[E\d+\])?:").unwrap();
    let rust_loc_re = Regex::new(r"^\s*-->\s+\S+:\d+:\d+").unwrap();
    let go_re = Regex::new(r"\.go:\d+:\d+:|^FAIL\s+").unwrap();
    let py_re = Regex::new(r"\.py:\d+:(\d+:)?\s*(error|Error)|^FAILED\s+").unwrap();

    let lang = language.to_ascii_lowercase();
    let mut matched = Vec::new();
    let mut total_bytes = 0usize;
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[0-?]*[ -/]*[@-~]").unwrap());
    static PRIVATE_PATH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(^|[\s"'`(=])(?:[A-Za-z]:[/\x5c]|/|~/)[^\s"'`)]+"#).unwrap()
    });
    let output = ANSI.replace_all(output, "");
    let lines: Vec<&str> = output.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || ContextStreamClient::secret_content_reason(trimmed).is_some() {
            continue;
        }
        let is_match = match lang.as_str() {
            "typescript" | "ts" | "javascript" | "js" => ts_re.is_match(trimmed),
            "rust" | "rs" => {
                rust_err_re.is_match(trimmed)
                    || (rust_loc_re.is_match(trimmed)
                        && idx > 0
                        && rust_err_re.is_match(lines[idx - 1].trim_end()))
            }
            "go" | "golang" => go_re.is_match(trimmed),
            "python" | "py" => py_re.is_match(trimmed),
            _ => {
                ts_re.is_match(trimmed)
                    || rust_err_re.is_match(trimmed)
                    || rust_loc_re.is_match(trimmed)
                    || go_re.is_match(trimmed)
                    || py_re.is_match(trimmed)
            }
        };
        if is_match {
            let candidate = PRIVATE_PATH
                .replace_all(trimmed, "${1}[redacted-path]")
                .into_owned();
            if total_bytes + candidate.len() > MAX_DIAGNOSTIC_BYTES {
                break;
            }
            total_bytes += candidate.len();
            matched.push(candidate);
            if matched.len() >= MAX_DIAGNOSTIC_LINES {
                break;
            }
        }
    }

    if matched.is_empty() {
        for line in lines.iter().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() || ContextStreamClient::secret_content_reason(trimmed).is_some() {
                continue;
            }
            let candidate = PRIVATE_PATH
                .replace_all(trimmed, "${1}[redacted-path]")
                .into_owned();
            if total_bytes + candidate.len() > MAX_DIAGNOSTIC_BYTES {
                break;
            }
            total_bytes += candidate.len();
            matched.push(candidate);
            if matched.len() >= 10 {
                break;
            }
        }
        matched.reverse();
    }

    matched
}

fn text_from_tool_result_frame(frame: &Value) -> String {
    frame
        .pointer("/tool_result/content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

async fn run_single_consumer(
    target: ConsumerContractTarget,
    peer_handle: Option<peer::CallPeer>,
    started: Instant,
) -> ConsumerContractOutcome {
    if let Some(inline) = target.result {
        let passed = inline.ran && inline.exit_code == Some(0);
        let errors = if inline.ran && !passed {
            extract_compiler_errors(&target.language, &inline.output)
        } else {
            Vec::new()
        };
        return ConsumerContractOutcome {
            project_id: target.project_id,
            project_name: target.project_name,
            repo_key: target.repo_key,
            default_branch: target.default_branch,
            language: target.language,
            command: target.command,
            ran: inline.ran,
            passed,
            exit_code: inline.exit_code,
            timed_out: inline.timed_out,
            refused_reason: inline.refused_reason,
            errors,
        };
    }

    let Some(spec) = target.stream else {
        return ConsumerContractOutcome {
            project_id: target.project_id,
            project_name: target.project_name,
            repo_key: target.repo_key,
            default_branch: target.default_branch,
            language: target.language,
            command: target.command,
            ran: false,
            passed: false,
            exit_code: None,
            timed_out: false,
            refused_reason: target
                .ticket_error
                .or_else(|| Some("no stream ticket issued".to_string())),
            errors: Vec::new(),
        };
    };

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let repo_label = target
        .project_name
        .as_deref()
        .unwrap_or(&target.repo_key)
        .to_string();
    let pump = async {
        while let Some(ev) = events_rx.recv().await {
            if let (Event::Heartbeat, Some(p)) = (ev, &peer_handle) {
                p.progress(
                    started.elapsed().as_secs_f64().max(1.0),
                    None,
                    Some(&format!("verifying consumer contract: {repo_label}")),
                )
                .await;
            }
        }
    };

    let run_fut = tokio::time::timeout(
        CONSUMER_RUN_TIMEOUT,
        drive_contract_stream(&spec, events_tx),
    );
    let (stream_res, ()) = tokio::join!(run_fut, pump);

    match stream_res {
        Err(_) => ConsumerContractOutcome {
            project_id: target.project_id,
            project_name: target.project_name,
            repo_key: target.repo_key,
            default_branch: target.default_branch,
            language: target.language,
            command: target.command,
            ran: false,
            passed: false,
            exit_code: None,
            timed_out: true,
            refused_reason: Some("consumer contract verification timed out".to_string()),
            errors: Vec::new(),
        },
        Ok(Err(reason)) => ConsumerContractOutcome {
            project_id: target.project_id,
            project_name: target.project_name,
            repo_key: target.repo_key,
            default_branch: target.default_branch,
            language: target.language,
            command: target.command,
            ran: false,
            passed: false,
            exit_code: None,
            timed_out: false,
            refused_reason: Some(reason),
            errors: Vec::new(),
        },
        Ok(Ok((frame, output_bytes))) => {
            if frame.get("ran").and_then(Value::as_bool) != Some(true) {
                let reason = frames::declined_reason(&frame)
                    .unwrap_or_else(|| "the service declined to run consumer check".to_string());
                return ConsumerContractOutcome {
                    project_id: target.project_id,
                    project_name: target.project_name,
                    repo_key: target.repo_key,
                    default_branch: target.default_branch,
                    language: target.language,
                    command: target.command,
                    ran: false,
                    passed: false,
                    exit_code: frame.get("exit").and_then(Value::as_i64).map(|e| e as i32),
                    timed_out: false,
                    refused_reason: Some(reason),
                    errors: Vec::new(),
                };
            }
            let exit_code = frame
                .get("exit")
                .and_then(Value::as_i64)
                .or_else(|| {
                    frame
                        .pointer("/tool_result/structuredContent/exit")
                        .and_then(Value::as_i64)
                })
                .unwrap_or_else(|| {
                    if frame
                        .pointer("/tool_result/isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        1
                    } else {
                        0
                    }
                }) as i32;
            let passed = exit_code == 0;
            let output_text = if !output_bytes.is_empty() {
                String::from_utf8_lossy(&output_bytes).into_owned()
            } else {
                text_from_tool_result_frame(&frame)
            };
            let errors = if passed {
                Vec::new()
            } else {
                extract_compiler_errors(&target.language, &output_text)
            };
            ConsumerContractOutcome {
                project_id: target.project_id,
                project_name: target.project_name,
                repo_key: target.repo_key,
                default_branch: target.default_branch,
                language: target.language,
                command: target.command,
                ran: true,
                passed,
                exit_code: Some(exit_code),
                timed_out: false,
                refused_reason: None,
                errors,
            }
        }
    }
}

pub fn should_run_contract_check(
    arguments: &Value,
    snapshot: Option<&CheckoutSnapshot>,
    result: &ToolResult,
) -> bool {
    let Some(snap) = snapshot else {
        return false;
    };
    if snap.overlay.is_empty() && snap.deletes.is_empty() {
        return false;
    }
    if !contract_check_env_enabled() {
        return false;
    }
    if arguments.get("prepare").and_then(Value::as_bool) == Some(true)
        || arguments.get("command").and_then(Value::as_str) == Some(":prepare")
        || arguments.get("speculative").and_then(Value::as_bool) == Some(true)
        || arguments.get("contract_check").and_then(Value::as_bool) == Some(false)
    {
        return false;
    }
    let forced = arguments.get("contract_check").and_then(Value::as_bool) == Some(true);
    let primary_ok = !result.is_error
        && result
            .structured_content
            .as_ref()
            .and_then(|s| s.get("ran"))
            .and_then(Value::as_bool)
            != Some(false);
    forced || primary_ok
}

pub(crate) async fn verify_consumer_contracts(
    client: &ContextStreamClient,
    session: &SessionManager,
    arguments: &Value,
    snapshot: Option<&CheckoutSnapshot>,
    result: &mut ToolResult,
) {
    if !should_run_contract_check(arguments, snapshot, result) {
        return;
    }
    let Some(snap) = snapshot else {
        return;
    };
    let Some(workspace_id) = listing_workspace(client, session).await else {
        return;
    };
    let project_id = session.state().await.project_id.map(|id| id.to_string());
    let (changed_paths, contract_files) = collect_contract_inputs(snap).await;
    if changed_paths.is_empty() {
        return;
    }

    let req = ContractCheckRequest {
        workspace_id: workspace_id.to_string(),
        project_id,
        repo_key: None,
        repo_name: Some(snap.repo_name.clone()),
        origin_sha256: Some(snap.origin_sha256.clone()),
        changed_paths,
        contract_files,
        ..Default::default()
    };

    let Ok(answer) = client
        .contract_check(&req, CONTRACT_CHECK_PLAN_TIMEOUT)
        .await
    else {
        return;
    };
    if !answer.is_success() {
        return;
    }
    let Ok(plan) = serde_json::from_value::<ContractCheckResponse>(answer.data) else {
        return;
    };
    if plan.consumers.is_empty() {
        return;
    }

    let consumers: Vec<ConsumerContractTarget> =
        plan.consumers.into_iter().take(MAX_CONSUMERS).collect();
    let peer_handle = peer::current().filter(peer::CallPeer::reports_progress);
    let started = Instant::now();
    if let Some(p) = &peer_handle {
        p.progress(
            1.0,
            Some((consumers.len() + 1) as f64),
            Some(&format!(
                "verifying {} downstream consumer contract(s)",
                consumers.len()
            )),
        )
        .await;
    }

    let outcomes = join_all(
        consumers
            .into_iter()
            .map(|target| run_single_consumer(target, peer_handle.clone(), started)),
    )
    .await;

    let failed: Vec<&ConsumerContractOutcome> =
        outcomes.iter().filter(|o| o.ran && !o.passed).collect();
    let unverified: Vec<&ConsumerContractOutcome> = outcomes.iter().filter(|o| !o.ran).collect();
    let all_passed = failed.is_empty() && unverified.is_empty();

    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    if let Some(obj) = structured.as_object_mut() {
        obj.insert(
            "consumer_contracts".to_string(),
            serde_json::to_value(&outcomes).unwrap_or_else(|_| json!([])),
        );
        obj.insert(
            "consumer_contracts_passed".to_string(),
            Value::Bool(all_passed),
        );
        obj.insert("consumer_contracts_failed".to_string(), json!(failed.len()));
    }

    if !failed.is_empty() {
        result.is_error = true;
        let mut lines = vec![format!(
            "[consumer contracts: {}/{} failed]",
            failed.len(),
            outcomes.len()
        )];
        for item in failed {
            let label = item.project_name.as_deref().unwrap_or(&item.repo_key);
            let exit = item.exit_code.unwrap_or(1);
            lines.push(format!(
                "- {label} ({}) `{}` exited with status {exit}:",
                item.repo_key, item.command
            ));
            for err in &item.errors {
                lines.push(format!("  {err}"));
            }
        }
        result
            .content
            .insert(0, ContentItem::text(lines.join("\n")));
    } else if !unverified.is_empty() {
        let reasons: Vec<String> = unverified
            .iter()
            .map(|o| {
                format!(
                    "{}: {}",
                    o.project_name.as_deref().unwrap_or(&o.repo_key),
                    o.refused_reason.as_deref().unwrap_or("unverified")
                )
            })
            .collect();
        result.content.push(ContentItem::text(format!(
            "⚠ consumer contracts: {}/{} unverified ({})",
            unverified.len(),
            outcomes.len(),
            reasons.join("; ")
        )));
    } else {
        let names: Vec<&str> = outcomes
            .iter()
            .map(|o| o.project_name.as_deref().unwrap_or(&o.repo_key))
            .collect();
        result.content.push(ContentItem::text(format!(
            "✓ consumer contracts: {}/{} passed ({})",
            outcomes.len(),
            outcomes.len(),
            names.join(", ")
        )));
    }
}
