//! Remote check settings and smart graph-driven verification (`verify` / `ms_verify`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::hosted_tools::{PlanCheckCommand, PlanCheckRequest, PlanCheckResponse};
use mcp_client::remote_checks::{
    indexed_filename, project_filenames, ProjectLanguages, RemoteChecks,
};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

pub const TOOL_NAME: &str = "verify";
pub const LEGACY_TOOL_NAME: &str = "ms_verify";

const PLAN_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const LOCAL_CMD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_TAIL_BYTES: usize = 8192;

const TOOL_DESCRIPTION: &str = "Run graph-driven verification checks for your current changes. Plans targeted tests via ContextStream's dependency/test graph (auto-escalating to full suite for lockfiles, migrations, auth, shared fixtures, high blast radius, or unmapped files) and runs them remotely when remote checks are enabled or locally as fallback.";

/// The new API applies both choices atomically. Older servers only return
/// `enabled`; use their dedicated engine route before reporting what changed.
pub async fn settings(
    client: &ContextStreamClient,
    workspace: Option<&str>,
    enabled: Option<bool>,
    more: Option<bool>,
) -> Result<RemoteChecks> {
    let result = match (enabled, more) {
        (Some(enabled), more) => {
            let mut state = client.set_remote_checks(enabled, more).await?;
            if state.more_engines.is_some() {
                Ok(state)
            } else if let Some(more) = more {
                match client.set_more_engines(more).await {
                    Ok(actual) => {
                        state.more_engines = Some(actual);
                        Ok(state)
                    }
                    Err(_) => Err(Error::Tool(format!(
                        "Remote checks: {}. Could not confirm the more engines setting.",
                        state.state()
                    ))),
                }
            } else {
                state.more_engines = client.more_engines().await.ok();
                Ok(state)
            }
        }
        (None, Some(more)) => {
            let actual = client.set_more_engines(more).await?;
            client.remote_checks().await.map(|mut state| {
                state.more_engines.get_or_insert(actual);
                state
            })
        }
        (None, None) => return client.remote_checks_status().await,
    };
    super::hosted_tools::refresh_settings(client, workspace).await;
    result
}

pub fn local_languages(folder: Option<&Path>, state: &RemoteChecks) -> Option<ProjectLanguages> {
    let names = project_filenames(folder?).ok()?;
    Some(ProjectLanguages::detect(&names, &state.languages))
}

/// The gateway cannot inspect its caller's filesystem. Use indexed filenames
/// for the selected project, with a deadline so an absent index or a slow API
/// never prevents the caller from reading or changing the account setting.
async fn indexed_languages(
    client: &ContextStreamClient,
    id: Uuid,
    folder: Option<&str>,
    state: &RemoteChecks,
) -> Option<ProjectLanguages> {
    let mut names = HashSet::new();
    let mut page = 1;
    let mut page_size = 100;
    let mut total = None;
    loop {
        let data = client
            .project_files_for_checkout(
                id,
                Some(page),
                Some(page_size),
                None,
                Some("path"),
                Some("asc"),
                folder,
            )
            .await
            .ok()?;
        let rows = data
            .get("files")
            .or_else(|| data.get("paths"))?
            .as_array()?;
        if page == 1 {
            total = data.get("total").and_then(Value::as_i64);
            if rows.is_empty() {
                return None;
            }
            page_size = rows.len() as i64; // Respect a lower server-side page cap.
        }
        names.extend(rows.iter().filter_map(|file| {
            let path = file
                .as_str()
                .or_else(|| file.get("path").or_else(|| file.get("file_path"))?.as_str())?;
            indexed_filename(path).map(str::to_string)
        }));
        if rows.len() < page_size as usize
            || total.is_some_and(|count| page * page_size >= count)
            || data.get("has_more").and_then(Value::as_bool) == Some(false)
        {
            break;
        }
        page += 1;
    }
    Some(ProjectLanguages::detect(&names, &state.languages))
}

pub fn report(state: &RemoteChecks, project: Option<&ProjectLanguages>) -> Value {
    let mut data = json!(state);
    data["project_languages"] = json!(project);
    data
}

pub(super) async fn session_action(
    client: &ContextStreamClient,
    session: &SessionManager,
    project: Option<Uuid>,
    enabled: Option<bool>,
    more: Option<bool>,
) -> Result<ToolResult> {
    let workspace = super::slipstream::listing_workspace(client, session)
        .await
        .map(|id| id.to_string());
    let state = match settings(client, workspace.as_deref(), enabled, more).await {
        Err(Error::Http { status: 403, .. }) => {
            return Ok(ToolResult::error(
                "Remote checks are unavailable for this account.",
            ))
        }
        other => other?,
    };
    let session = session.state().await;
    let config = client.config().await;
    let project = if config.is_http_transport {
        let id = project
            .or(session.project_id)
            .or_else(|| mcp_client::get_task_auth_override()?.project_id);
        match id {
            Some(id) => tokio::time::timeout(
                Duration::from_secs(3),
                indexed_languages(client, id, session.folder_path.as_deref(), &state),
            )
            .await
            .ok()
            .flatten(),
            None => None,
        }
    } else {
        let folder = session
            .folder_path
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        local_languages(folder.as_deref(), &state)
    };
    Ok(ToolResult::with_structured(
        state.status_text(project.as_ref()),
        report(&state, project.as_ref()),
    ))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VerifyInput {
    pub changed_paths: Option<Vec<String>>,
    pub scope: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalRunOutcome {
    exit_code: i32,
    stdout_tail: String,
    stderr_tail: String,
    timed_out: bool,
}

#[derive(Debug, Clone)]
enum ExecOutcome {
    Executed {
        passed: bool,
        exit_code: Option<i32>,
        ran_remotely: bool,
        output_tail: String,
    },
    FallbackLocal,
}

pub struct VerifyTool {
    client: Arc<ContextStreamClient>,
    session: Arc<SessionManager>,
    metadata: ToolMetadata,
}

impl VerifyTool {
    pub fn new(client: Arc<ContextStreamClient>, session: Arc<SessionManager>) -> Self {
        Self {
            client,
            session,
            metadata: ToolMetadata {
                name: TOOL_NAME.to_string(),
                title: "Verify Changes".to_string(),
                description: TOOL_DESCRIPTION.to_string(),
                category: ToolCategory::Utility,
                annotations: ToolAnnotations::read_only().closed_world().long_running(),
                is_pro: false,
                required_tier: None,
            },
        }
    }

    async fn resolve_cwd(&self, raw_cwd: Option<&str>) -> std::result::Result<PathBuf, String> {
        let session_folder = self.session.state().await.folder_path.map(PathBuf::from);
        let candidate = match raw_cwd.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let path = PathBuf::from(raw);
                if path.is_absolute() {
                    path
                } else if let Some(base) = session_folder {
                    base.join(path)
                } else if let Ok(curr) = std::env::current_dir() {
                    curr.join(path)
                } else {
                    path
                }
            }
            None => session_folder
                .or_else(|| std::env::current_dir().ok())
                .ok_or_else(|| {
                    "verify requires a local checkout folder or explicit `cwd`.".to_string()
                })?,
        };
        if !candidate.is_dir() {
            return Err(format!(
                "verify: `cwd` does not exist or is not a directory: {}",
                candidate.display()
            ));
        }
        Ok(candidate)
    }

    async fn resolve_workspace_id(&self) -> Option<Uuid> {
        if let Some(ws) = super::slipstream::listing_workspace(&self.client, &self.session).await {
            return Some(ws);
        }
        let state = self.session.state().await;
        if let Some(ws) = state.workspace_id {
            return Some(ws);
        }
        if let Some(ws) = mcp_client::get_task_auth_override().and_then(|a| a.workspace_id) {
            return Some(ws);
        }
        self.client.config().await.default_workspace_id
    }

    async fn resolve_project_id(&self) -> Option<Uuid> {
        let state = self.session.state().await;
        if let Some(pid) = state.project_id {
            return Some(pid);
        }
        if let Some(pid) = mcp_client::get_task_auth_override().and_then(|a| a.project_id) {
            return Some(pid);
        }
        self.client.config().await.default_project_id
    }
}

pub fn register_remote_checks_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: Arc<ContextStreamClient>,
    session: Arc<SessionManager>,
) {
    registry.register(TOOL_NAME, Arc::new(VerifyTool::new(client, session)));
    registry.register_alias(LEGACY_TOOL_NAME, TOOL_NAME);
}

#[async_trait]
impl ToolHandler for VerifyTool {
    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .array(
                "changed_paths",
                "Optional repository-relative changed file paths. If omitted, discovered from local git status/diff.",
                "string",
                false,
            )
            .string_enum(
                "scope",
                "Verification scope: targeted (graph-selected checks, default) or full (entire suite).",
                &["targeted", "full"],
                false,
            )
            .string(
                "cwd",
                "Optional working directory override for local checkout discovery and execution.",
                false,
            )
            .build()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let parsed: VerifyInput = serde_json::from_value(input)
            .map_err(|e| Error::Validation(format!("Invalid verify input: {e}")))?;

        let scope = parsed
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("targeted")
            .to_ascii_lowercase();
        if !matches!(scope.as_str(), "targeted" | "full") {
            return Err(Error::Validation(
                "`scope` must be `targeted` or `full`.".to_string(),
            ));
        }

        let cwd = match self.resolve_cwd(parsed.cwd.as_deref()).await {
            Ok(dir) => dir,
            Err(msg) => return Ok(ToolResult::error(msg)),
        };

        let explicit_cwd = parsed
            .cwd
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());

        let (exec_dir, repo_key, discovered_paths) =
            discover_checkout_context(&cwd, parsed.changed_paths.is_none()).await;

        let (changed_paths, has_unsafe_path) = match parsed.changed_paths {
            Some(ref paths) => normalize_and_validate_paths(paths),
            None => normalize_and_validate_paths(&discovered_paths),
        };

        let session_folder = self.session.state().await.folder_path;
        let cwd_matches_session = if !explicit_cwd {
            true
        } else if let Some(sf) = session_folder.as_deref() {
            let s_canon = std::fs::canonicalize(sf).unwrap_or_else(|_| PathBuf::from(sf));
            let s_root = git_toplevel(&s_canon)
                .await
                .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
                .unwrap_or(s_canon);
            let e_canon = std::fs::canonicalize(&exec_dir).unwrap_or_else(|_| exec_dir.clone());
            s_root == e_canon
        } else {
            false
        };

        let workspace_id = self.resolve_workspace_id().await;
        let project_id = if cwd_matches_session {
            self.resolve_project_id().await
        } else {
            None
        };

        let mut tier = "full".to_string();
        let mut escalation_reasons: Vec<String> = Vec::new();
        let mut plan_covering_tests: Vec<String> = Vec::new();
        let mut plan_co_changed_tests: Vec<String> = Vec::new();
        let mut downstream_count: usize = 0;

        let (reason, mut commands) = if scope == "full" {
            escalation_reasons.push("scope_full".to_string());
            if has_unsafe_path {
                escalation_reasons.push("unsafe_path".to_string());
            }
            (
                "Full verification requested via scope=full.".to_string(),
                detect_local_full_commands(&exec_dir),
            )
        } else if has_unsafe_path {
            escalation_reasons.push("unsafe_path".to_string());
            (
                "Escalated to full verification: unsafe_path".to_string(),
                detect_local_full_commands(&exec_dir),
            )
        } else if let Some(ws_id) = workspace_id {
            let req = PlanCheckRequest {
                workspace_id: ws_id.to_string(),
                project_id: project_id.map(|p| p.to_string()),
                repo_key: repo_key.clone(),
                changed_paths: changed_paths.clone(),
                scope: Some("targeted".to_string()),
                max_downstream: None,
            };
            match tokio::time::timeout(
                PLAN_CHECK_TIMEOUT,
                self.client.plan_check(&req, PLAN_CHECK_TIMEOUT),
            )
            .await
            {
                Ok(Ok(answer)) if answer.is_success() => {
                    match serde_json::from_value::<PlanCheckResponse>(answer.data) {
                        Ok(plan) => {
                            tier = if plan.tier.is_empty() {
                                "full".to_string()
                            } else {
                                plan.tier
                            };
                            escalation_reasons = plan.escalation_reasons;
                            plan_covering_tests = plan.covering_tests;
                            plan_co_changed_tests = plan.co_changed_tests;
                            downstream_count = plan.downstream_count;
                            let cmds = if tier == "targeted" || !plan.commands.is_empty() {
                                plan.commands
                            } else if !plan.fallback_commands.is_empty() {
                                plan.fallback_commands
                            } else {
                                detect_local_full_commands(&exec_dir)
                            };
                            (plan.reason, cmds)
                        }
                        Err(err) => {
                            escalation_reasons.push("plan_check_invalid_response".to_string());
                            (
                                format!(
                                    "plan-check returned unreadable payload ({err}); falling back to local full suite."
                                ),
                                detect_local_full_commands(&exec_dir),
                            )
                        }
                    }
                }
                Ok(Ok(answer)) => {
                    escalation_reasons.push(format!("plan_check_http_{}", answer.status));
                    (
                        format!(
                            "plan-check returned HTTP {}; falling back to local full suite.",
                            answer.status
                        ),
                        detect_local_full_commands(&exec_dir),
                    )
                }
                Ok(Err(err)) => {
                    escalation_reasons.push("plan_check_unreachable".to_string());
                    (
                        format!(
                            "plan-check unreachable ({err}); falling back to local full suite."
                        ),
                        detect_local_full_commands(&exec_dir),
                    )
                }
                Err(_) => {
                    escalation_reasons.push("plan_check_timeout".to_string());
                    (
                        "plan-check timed out; falling back to local full suite.".to_string(),
                        detect_local_full_commands(&exec_dir),
                    )
                }
            }
        } else {
            escalation_reasons.push("no_workspace".to_string());
            (
                "No workspace_id resolved; falling back to local full suite.".to_string(),
                detect_local_full_commands(&exec_dir),
            )
        };

        if commands.is_empty() {
            commands = detect_local_full_commands(&exec_dir);
            if tier == "targeted" {
                tier = "full".to_string();
                if !escalation_reasons.iter().any(|r| r == "no_mapped_tests") {
                    escalation_reasons.push("no_mapped_tests".to_string());
                }
            }
        }

        if commands.is_empty() {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let payload = json!({
                "tier": tier,
                "scope": scope,
                "reason": reason,
                "escalation_reasons": escalation_reasons,
                "changed_paths": changed_paths,
                "covering_tests": plan_covering_tests,
                "co_changed_tests": plan_co_changed_tests,
                "downstream_count": downstream_count,
                "ran_remotely": false,
                "commands_run": [],
                "passed": false,
                "failed_command": Value::Null,
                "exit_code": Value::Null,
                "elapsed_ms": elapsed_ms,
            });
            let mut result = ToolResult::with_structured(
                format!(
                    "✗ verify found no runnable verification commands in {}",
                    exec_dir.display()
                ),
                payload,
            );
            result.is_error = true;
            return Ok(result);
        }

        let hosted_check = if cwd_matches_session {
            super::hosted_tools::find(
                &super::hosted_tools::HostedToolsGate::global(),
                &self.client,
                &self.session,
                "check",
            )
            .await
        } else {
            None
        };

        let mut commands_run: Vec<Value> = Vec::new();
        let mut all_passed = true;
        let mut any_remote = false;
        let mut failed_command: Option<String> = None;
        let mut failed_exit_code: Option<i32> = None;
        let mut failure_output = String::new();

        for cmd in &commands {
            let cmd_started = Instant::now();
            let cmd_cwd = cmd
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|sub| exec_dir.join(sub))
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| exec_dir.clone());

            let outcome = if let Some(ref tool) = hosted_check {
                let args = match cmd.cwd.as_deref().filter(|s| !s.is_empty()) {
                    Some(sub) => json!({ "command": cmd.command, "cwd": sub }),
                    None => json!({ "command": cmd.command }),
                };
                let remote_res =
                    super::hosted_tools::run(&self.client, &self.session, tool, args).await;
                interpret_hosted_check_outcome(&remote_res)
            } else {
                ExecOutcome::FallbackLocal
            };

            let (passed, exit_code, ran_remotely, output_tail) = match outcome {
                ExecOutcome::Executed {
                    passed,
                    exit_code,
                    ran_remotely,
                    output_tail,
                } => (passed, exit_code, ran_remotely, output_tail),
                ExecOutcome::FallbackLocal => {
                    let local = run_local_command(&cmd.command, &cmd_cwd, LOCAL_CMD_TIMEOUT).await;
                    let tail = format_local_output(&local);
                    (
                        !local.timed_out && local.exit_code == 0,
                        Some(local.exit_code),
                        false,
                        tail,
                    )
                }
            };

            if ran_remotely {
                any_remote = true;
            }
            let cmd_elapsed_ms = cmd_started.elapsed().as_millis() as u64;
            commands_run.push(json!({
                "command": cmd.command,
                "language": cmd.language,
                "tier": cmd.tier,
                "targets": cmd.targets,
                "ran_remotely": ran_remotely,
                "passed": passed,
                "exit_code": exit_code,
                "elapsed_ms": cmd_elapsed_ms,
            }));

            if !passed {
                all_passed = false;
                failed_command = Some(cmd.command.clone());
                failed_exit_code = exit_code;
                failure_output = output_tail;
                break;
            }
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let payload = json!({
            "tier": tier,
            "scope": scope,
            "reason": reason,
            "escalation_reasons": escalation_reasons,
            "changed_paths": changed_paths,
            "covering_tests": plan_covering_tests,
            "co_changed_tests": plan_co_changed_tests,
            "downstream_count": downstream_count,
            "ran_remotely": any_remote,
            "commands_run": commands_run,
            "passed": all_passed,
            "failed_command": failed_command,
            "exit_code": failed_exit_code,
            "elapsed_ms": elapsed_ms,
        });

        let summary = if all_passed {
            format!(
                "✓ verify ({tier}) passed · {} command(s) · {} ms · {}",
                commands_run.len(),
                elapsed_ms,
                reason
            )
        } else {
            let cmd_name = failed_command.as_deref().unwrap_or("<unknown>");
            let code_label = failed_exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            if failure_output.is_empty() {
                format!(
                    "✗ verify ({tier}) failed on `{cmd_name}` (exit {code_label}) · {elapsed_ms} ms"
                )
            } else {
                format!(
                    "✗ verify ({tier}) failed on `{cmd_name}` (exit {code_label}) · {elapsed_ms} ms\n\n{failure_output}"
                )
            }
        };

        let mut result = ToolResult::with_structured(summary, payload);
        result.is_error = !all_passed;
        Ok(result)
    }
}

pub(crate) fn is_safe_in_tree_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('~') || path.starts_with('-') {
        return false;
    }
    if path.split('/').any(|seg| seg == "..") || path.contains("..") {
        return false;
    }
    path.bytes().all(|b| {
        matches!(
            b,
            b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'='
                | b':'
                | b','
                | b'.'
                | b'_'
                | b'/'
                | b'@'
                | b'+'
                | b'-'
        )
    })
}

fn normalize_and_validate_paths(raw_paths: &[String]) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut has_unsafe = false;
    for raw in raw_paths {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let slash_norm = trimmed.replace('\\', "/");
        let norm = slash_norm
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if norm.is_empty() {
            if !is_safe_in_tree_path(&slash_norm) {
                has_unsafe = true;
            }
            continue;
        }
        if !is_safe_in_tree_path(&norm) {
            has_unsafe = true;
        }
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    (out, has_unsafe)
}

async fn discover_checkout_context(
    cwd: &Path,
    discover_changes: bool,
) -> (PathBuf, Option<String>, Vec<String>) {
    match CheckoutSnapshot::begin(cwd).await {
        Ok(start) => {
            let root = start.root().to_path_buf();
            let repo_key = start
                .identity()
                .pointer("/repo/origin_sha256")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut paths = Vec::new();
            if discover_changes {
                if let Ok(snapshot) = start.complete().await {
                    for entry in &snapshot.overlay {
                        if !paths.contains(&entry.path) {
                            paths.push(entry.path.clone());
                        }
                    }
                    for deleted in &snapshot.deletes {
                        if !paths.contains(deleted) {
                            paths.push(deleted.clone());
                        }
                    }
                }
                if paths.is_empty() {
                    paths = git_changed_paths(&root).await;
                }
            }
            (root, repo_key, paths)
        }
        Err(_) => {
            let root = git_toplevel(cwd).await.unwrap_or_else(|| cwd.to_path_buf());
            let paths = if discover_changes {
                git_changed_paths(&root).await
            } else {
                Vec::new()
            };
            (root, None, paths)
        }
    }
}

async fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

async fn git_changed_paths(repo_root: &Path) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();

    if let Ok(status_out) = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])
        .output()
        .await
    {
        if status_out.status.success() {
            for entry in status_out.stdout.split(|b| *b == 0) {
                if entry.len() > 3 {
                    let path = String::from_utf8_lossy(&entry[3..]).trim().to_string();
                    if !path.is_empty() && !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }
    }

    if let Ok(diff_out) = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--name-only", "HEAD"])
        .output()
        .await
    {
        if diff_out.status.success() {
            for line in String::from_utf8_lossy(&diff_out.stdout).lines() {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() && !paths.contains(&trimmed) {
                    paths.push(trimmed);
                }
            }
        }
    }

    paths
}

fn package_json_scripts_mention(package_json: &Path, needle: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(package_json) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    parsed
        .get("scripts")
        .and_then(Value::as_object)
        .is_some_and(|scripts| {
            scripts.values().any(|v| {
                v.as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains(needle))
            })
        })
}

pub(crate) fn detect_local_full_commands(root: &Path) -> Vec<PlanCheckCommand> {
    let mut commands = Vec::new();
    if root.join("Cargo.toml").is_file() {
        commands.push(PlanCheckCommand {
            command: "cargo test".to_string(),
            cwd: None,
            language: "rust".to_string(),
            targets: Vec::new(),
            tier: "full".to_string(),
        });
    }
    let package_json = root.join("package.json");
    if package_json.is_file() {
        let has_vitest = [
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mts",
            "vitest.config.mjs",
            "vitest.config.cts",
            "vitest.config.cjs",
        ]
        .iter()
        .any(|name| root.join(name).is_file());
        let has_jest = [
            "jest.config.ts",
            "jest.config.js",
            "jest.config.mts",
            "jest.config.mjs",
            "jest.config.cts",
            "jest.config.cjs",
            "jest.config.json",
        ]
        .iter()
        .any(|name| root.join(name).is_file());
        let command = if has_vitest {
            "npx vitest run".to_string()
        } else if has_jest {
            "npx jest".to_string()
        } else if package_json_scripts_mention(&package_json, "vitest") {
            "npx vitest run".to_string()
        } else {
            "npm test".to_string()
        };
        commands.push(PlanCheckCommand {
            command,
            cwd: None,
            language: "typescript".to_string(),
            targets: Vec::new(),
            tier: "full".to_string(),
        });
    }
    if root.join("pyproject.toml").is_file() {
        commands.push(PlanCheckCommand {
            command: "uv run pytest".to_string(),
            cwd: None,
            language: "python".to_string(),
            targets: Vec::new(),
            tier: "full".to_string(),
        });
    }
    if root.join("go.mod").is_file() {
        commands.push(PlanCheckCommand {
            command: "go test ./...".to_string(),
            cwd: None,
            language: "go".to_string(),
            targets: Vec::new(),
            tier: "full".to_string(),
        });
    }
    commands
}

fn interpret_hosted_check_outcome(result: &ToolResult) -> ExecOutcome {
    let ran_flag = result
        .structured_content
        .as_ref()
        .and_then(|s| s.get("ran"))
        .and_then(Value::as_bool);

    let text = result
        .content
        .iter()
        .filter_map(|item| match item {
            mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if ran_flag == Some(false)
        || text.starts_with(super::hosted_tools::CREDIT_MESSAGE)
        || (result.is_error
            && ran_flag != Some(true)
            && (text.contains("was not run: it needs a snapshot of this checkout")
                || text.contains("was not run: the snapshot of this checkout")
                || text.contains("could not be reached:")
                || text.contains("check failed (HTTP 502)")
                || text.contains("check failed (HTTP 503)")
                || text.contains("check failed (HTTP 504)")
                || text.contains("the API named no stream for this call")
                || text.contains("the API described no run for this call")))
    {
        return ExecOutcome::FallbackLocal;
    }

    let exit_code = result
        .structured_content
        .as_ref()
        .and_then(|s| s.get("exit"))
        .and_then(Value::as_i64)
        .map(|v| v as i32);

    let passed = !result.is_error && exit_code.unwrap_or(0) == 0;
    ExecOutcome::Executed {
        passed,
        exit_code,
        ran_remotely: true,
        output_tail: tail_utf8(&text, MAX_OUTPUT_TAIL_BYTES),
    }
}

async fn run_local_command(command: &str, cwd: &Path, timeout: Duration) -> LocalRunOutcome {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => LocalRunOutcome {
            exit_code: output.status.code().unwrap_or(1),
            stdout_tail: tail_utf8(
                &String::from_utf8_lossy(&output.stdout),
                MAX_OUTPUT_TAIL_BYTES,
            ),
            stderr_tail: tail_utf8(
                &String::from_utf8_lossy(&output.stderr),
                MAX_OUTPUT_TAIL_BYTES,
            ),
            timed_out: false,
        },
        Ok(Err(err)) => LocalRunOutcome {
            exit_code: 127,
            stdout_tail: String::new(),
            stderr_tail: format!("failed to execute `{command}`: {err}"),
            timed_out: false,
        },
        Err(_) => LocalRunOutcome {
            exit_code: 124,
            stdout_tail: String::new(),
            stderr_tail: format!("command `{command}` timed out after {}s", timeout.as_secs()),
            timed_out: true,
        },
    }
}

fn tail_utf8(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - max_bytes;
    while !trimmed.is_char_boundary(start) && start < trimmed.len() {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

fn format_local_output(out: &LocalRunOutcome) -> String {
    let mut parts = Vec::new();
    if !out.stdout_tail.is_empty() {
        parts.push(out.stdout_tail.clone());
    }
    if !out.stderr_tail.is_empty() {
        parts.push(out.stderr_tail.clone());
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use crate::testing::TestFixtures;
    use mcp_types::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_plan_check_server(
        status: u16,
        response_json: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let resp_body = response_json.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (resp_status, payload) = if req.contains("GET /api/v1/mcp/hosted-tools") {
                        (
                            200,
                            json!({"success": true, "data": {"tools": []}}).to_string(),
                        )
                    } else if req.contains("POST /api/v1/mcp/hosted-tools/plan-check") {
                        (status, resp_body.to_string())
                    } else {
                        (
                            404,
                            json!({"success": false, "error": {"message": "not found"}})
                                .to_string(),
                        )
                    };
                    let status_text = match resp_status {
                        200 => "200 OK",
                        404 => "404 Not Found",
                        _ => "500 Internal Server Error",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn init_cargo_workspace_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        for crate_name in ["alpha", "beta"] {
            let cdir = dir.path().join("crates").join(crate_name);
            std::fs::create_dir_all(cdir.join("src")).unwrap();
            std::fs::write(
                cdir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                ),
            )
            .unwrap();
            std::fs::write(
                cdir.join("src/lib.rs"),
                format!(
                    "pub fn value() -> &'static str {{ \"{crate_name}\" }}\n#[cfg(test)]\nmod tests {{ #[test] fn ok() {{ assert_eq!(super::value(), \"{crate_name}\"); }} }}\n"
                ),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn verify_and_ms_verify_alias_register_cleanly() {
        let config = TestFixtures::test_config();
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config.clone()));
        let mut registry = ToolRegistry::new(&config);
        register_remote_checks_tools(&mut registry, client, session);

        assert!(registry.get("verify").is_some());
        assert!(registry.get("ms_verify").is_some());
        assert_eq!(registry.get("ms_verify").unwrap().metadata.name, "verify");
        let names = registry.names();
        assert!(names.contains(&"verify"));
        assert!(!names.contains(&"ms_verify"));
    }

    #[tokio::test]
    async fn verify_runs_targeted_cargo_test_and_preserves_structured_content() {
        let fixture = init_cargo_workspace_fixture();
        let ws = Uuid::new_v4();
        let (api_url, _server) = spawn_mock_plan_check_server(
            200,
            json!({
                "success": true,
                "data": {
                    "tier": "targeted",
                    "reason": "Selected 1 targeted crate",
                    "escalation_reasons": [],
                    "project_id": null,
                    "downstream_count": 1,
                    "covering_tests": ["crates/alpha/src/lib.rs"],
                    "co_changed_tests": [],
                    "commands": [{
                        "command": "cargo test -p alpha",
                        "cwd": null,
                        "language": "rust",
                        "targets": ["crates/alpha/src/lib.rs"],
                        "tier": "targeted"
                    }],
                    "fallback_commands": [{
                        "command": "cargo test",
                        "cwd": null,
                        "language": "rust",
                        "targets": [],
                        "tier": "full"
                    }],
                    "elapsed_ms": 2
                }
            }),
        )
        .await;

        let config = Config {
            api_url,
            api_key: Some("test-key".to_string()),
            default_workspace_id: Some(ws),
            ..Config::default()
        };
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config.clone()));
        let mut registry = ToolRegistry::new(&config);
        register_remote_checks_tools(&mut registry, client, session);

        let result = registry
            .execute(
                "ms_verify",
                json!({
                    "changed_paths": ["crates/alpha/src/lib.rs"],
                    "scope": "targeted",
                    "cwd": fixture.path().to_string_lossy(),
                }),
            )
            .await
            .expect("verify execution");

        assert!(!result.is_error, "result failed: {:?}", result.content);
        let structured = result.structured_content.expect("structuredContent");
        assert_eq!(structured["tier"], "targeted");
        assert_eq!(structured["passed"], true);
        assert_eq!(structured["ran_remotely"], false);
        assert_eq!(structured["commands_run"].as_array().unwrap().len(), 1);
        assert_eq!(
            structured["commands_run"][0]["command"],
            "cargo test -p alpha"
        );
    }

    #[tokio::test]
    async fn verify_falls_back_to_local_full_suite_when_plan_check_returns_404() {
        let fixture = init_cargo_workspace_fixture();
        let ws = Uuid::new_v4();
        let (api_url, _server) = spawn_mock_plan_check_server(
            404,
            json!({ "success": false, "error": { "message": "not found" } }),
        )
        .await;

        let config = Config {
            api_url,
            api_key: Some("test-key".to_string()),
            default_workspace_id: Some(ws),
            ..Config::default()
        };
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config));
        let tool = VerifyTool::new(client, session);

        let result = tool
            .execute(json!({
                "changed_paths": ["Cargo.lock"],
                "scope": "targeted",
                "cwd": fixture.path().to_string_lossy(),
            }))
            .await
            .expect("verify fallback");

        assert!(!result.is_error, "fallback failed: {:?}", result.content);
        let structured = result.structured_content.expect("structuredContent");
        assert_eq!(structured["tier"], "full");
        assert_eq!(structured["passed"], true);
        assert_eq!(structured["commands_run"][0]["command"], "cargo test");
        assert_eq!(
            structured["escalation_reasons"],
            json!(["plan_check_http_404"])
        );
    }

    #[tokio::test]
    async fn verify_stops_on_first_failure_with_diagnostics() {
        let fixture = init_cargo_workspace_fixture();
        let ws = Uuid::new_v4();
        let (api_url, _server) = spawn_mock_plan_check_server(
            200,
            json!({
                "success": true,
                "data": {
                    "tier": "targeted",
                    "reason": "Selected 2 targeted commands",
                    "escalation_reasons": [],
                    "commands": [
                        {
                            "command": "echo first-ok",
                            "cwd": null,
                            "language": "rust",
                            "targets": [],
                            "tier": "targeted"
                        },
                        {
                            "command": "echo boom >&2 && exit 7",
                            "cwd": null,
                            "language": "rust",
                            "targets": [],
                            "tier": "targeted"
                        },
                        {
                            "command": "echo should-not-run",
                            "cwd": null,
                            "language": "rust",
                            "targets": [],
                            "tier": "targeted"
                        }
                    ]
                }
            }),
        )
        .await;

        let config = Config {
            api_url,
            api_key: Some("test-key".to_string()),
            default_workspace_id: Some(ws),
            ..Config::default()
        };
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config));
        let tool = VerifyTool::new(client, session);

        let result = tool
            .execute(json!({
                "changed_paths": ["crates/alpha/src/lib.rs"],
                "cwd": fixture.path().to_string_lossy(),
            }))
            .await
            .expect("verify failure result");

        assert!(result.is_error);
        let structured = result.structured_content.expect("structuredContent");
        assert_eq!(structured["passed"], false);
        assert_eq!(structured["exit_code"], 7);
        assert_eq!(structured["failed_command"], "echo boom >&2 && exit 7");
        assert_eq!(structured["commands_run"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn interpret_hosted_check_falls_back_on_not_run_or_out_of_credits() {
        let not_run = ToolResult {
            content: vec![mcp_types::tool::ContentItem::text("not run")],
            structured_content: Some(json!({ "ran": false })),
            is_error: true,
            meta: None,
        };
        assert!(matches!(
            interpret_hosted_check_outcome(&not_run),
            ExecOutcome::FallbackLocal
        ));

        let no_credits = ToolResult::error(format!(
            "{} Run the command locally.",
            crate::domains::hosted_tools::CREDIT_MESSAGE
        ));
        assert!(matches!(
            interpret_hosted_check_outcome(&no_credits),
            ExecOutcome::FallbackLocal
        ));

        let ok_remote = ToolResult {
            content: vec![mcp_types::tool::ContentItem::text("ok")],
            structured_content: Some(json!({ "ran": true, "exit": 0 })),
            is_error: false,
            meta: None,
        };
        assert!(matches!(
            interpret_hosted_check_outcome(&ok_remote),
            ExecOutcome::Executed {
                passed: true,
                ran_remotely: true,
                ..
            }
        ));
    }

    #[test]
    fn interpret_hosted_check_falls_back_on_5xx_and_snapshot_join_failures_only() {
        for err_msg in [
            "check failed (HTTP 502)",
            "check failed (HTTP 503)",
            "check failed (HTTP 504)",
            "check was not run: the snapshot of this checkout could not be taken",
            "check was not run: the snapshot of this checkout was not taken",
            "check: the API named no stream for this call",
            "check: the API described no run for this call",
        ] {
            let res = ToolResult::error(err_msg);
            assert!(
                matches!(
                    interpret_hosted_check_outcome(&res),
                    ExecOutcome::FallbackLocal
                ),
                "expected local fallback for `{err_msg}`"
            );
        }

        // Real check failures (especially with ran: true) must remain remote failures.
        let real_fail = ToolResult {
            content: vec![mcp_types::tool::ContentItem::text(
                "test result: FAILED. check failed (HTTP 503) in mock test",
            )],
            structured_content: Some(json!({ "ran": true, "exit": 101 })),
            is_error: true,
            meta: None,
        };
        assert!(matches!(
            interpret_hosted_check_outcome(&real_fail),
            ExecOutcome::Executed {
                passed: false,
                exit_code: Some(101),
                ran_remotely: true,
                ..
            }
        ));

        let plain_fail = ToolResult::error("check: the command exited with status 101.");
        assert!(matches!(
            interpret_hosted_check_outcome(&plain_fail),
            ExecOutcome::Executed {
                passed: false,
                ran_remotely: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn verify_escalates_unsafe_changed_paths_to_full_without_shell_interpolation() {
        let fixture = init_cargo_workspace_fixture();
        let ws = Uuid::new_v4();
        let sentinel = fixture.path().join("pwned_sentinel");

        // Even if a mock plan-check server would return "targeted", VerifyTool must fail-safe escalate.
        let (api_url, _server) = spawn_mock_plan_check_server(
            200,
            json!({
                "success": true,
                "data": {
                    "tier": "targeted",
                    "reason": "Selected 1 targeted crate",
                    "escalation_reasons": [],
                    "commands": [{
                        "command": "cargo test -p alpha",
                        "cwd": null,
                        "language": "rust",
                        "targets": ["crates/alpha/src/lib.rs"],
                        "tier": "targeted"
                    }]
                }
            }),
        )
        .await;

        let config = Config {
            api_url,
            api_key: Some("test-key".to_string()),
            default_workspace_id: Some(ws),
            ..Config::default()
        };
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config));
        let tool = VerifyTool::new(client, session);

        let result = tool
            .execute(json!({
                "changed_paths": [
                    "../etc/passwd",
                    "/etc/passwd",
                    "foo; rm -rf /",
                    format!("$(touch {}).rs", sentinel.display()),
                    "-flag.test.ts",
                    "web/src/a.test.ts"
                ],
                "scope": "targeted",
                "cwd": fixture.path().to_string_lossy(),
            }))
            .await
            .expect("verify unsafe paths");

        assert!(!sentinel.exists(), "shell injection occurred!");
        assert!(!result.is_error, "verify failed: {:?}", result.content);
        let structured = result.structured_content.expect("structuredContent");
        assert_eq!(structured["tier"], "full");
        assert_eq!(structured["passed"], true);
        let reasons = structured["escalation_reasons"]
            .as_array()
            .expect("escalation_reasons");
        assert!(reasons.iter().any(|r| r == "unsafe_path"));
        assert_eq!(structured["commands_run"][0]["command"], "cargo test");
    }

    #[tokio::test]
    async fn verify_omits_project_id_when_cwd_differs_from_session_folder() {
        let repo_a = init_cargo_workspace_fixture();
        let repo_b = init_cargo_workspace_fixture();
        let init_git = |dir: &Path, remote_url: &str| {
            let run = |args: &[&str]| {
                assert!(std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success());
            };
            run(&["init", "-q"]);
            run(&["remote", "add", "origin", remote_url]);
            run(&["add", "."]);
            run(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "init",
            ]);
        };
        init_git(repo_a.path(), "https://example.com/org/repo_a.git");
        init_git(repo_b.path(), "https://example.com/org/repo_b.git");

        let expected_repo_b_key = discover_checkout_context(repo_b.path(), false)
            .await
            .1
            .expect("repo_b origin_sha256");

        let captured_requests: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_clone = captured_requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let _server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let captured = captured_clone.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (status_text, payload) = if req.contains("GET /api/v1/mcp/hosted-tools") {
                        (
                            "200 OK",
                            json!({"success": true, "data": {"tools": []}}).to_string(),
                        )
                    } else if req.contains("POST /api/v1/mcp/hosted-tools/plan-check") {
                        if let Some((_, body)) = req.split_once("\r\n\r\n") {
                            if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                                captured.lock().await.push(parsed);
                            }
                        }
                        (
                            "200 OK",
                            json!({
                                "success": true,
                                "data": {
                                    "tier": "targeted",
                                    "reason": "Selected 1 targeted crate",
                                    "escalation_reasons": [],
                                    "commands": [{
                                        "command": "cargo test -p alpha",
                                        "cwd": null,
                                        "language": "rust",
                                        "targets": ["crates/alpha/src/lib.rs"],
                                        "tier": "targeted"
                                    }]
                                }
                            })
                            .to_string(),
                        )
                    } else {
                        ("404 Not Found", json!({"success": false}).to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let ws = Uuid::new_v4();
        let proj_a = Uuid::new_v4();
        let config = Config {
            api_url: format!("http://{addr}"),
            api_key: Some("test-key".to_string()),
            default_workspace_id: Some(ws),
            ..Config::default()
        };
        let raw_client = ContextStreamClient::new(config.clone());
        let client = Arc::new(raw_client.clone());
        let session = Arc::new(SessionManager::new(raw_client, config));
        session
            .initialize(
                Some(ws),
                Some(proj_a),
                Some(repo_a.path().to_string_lossy().to_string()),
                None,
            )
            .await;

        let tool = VerifyTool::new(client, session);

        // Calling verify on repo_b must omit project_id and send repo_b's repo_key.
        let res_b = tool
            .execute(json!({
                "changed_paths": ["crates/alpha/src/lib.rs"],
                "scope": "targeted",
                "cwd": repo_b.path().to_string_lossy(),
            }))
            .await
            .expect("verify repo_b");
        assert!(!res_b.is_error);

        // Calling verify on repo_a (matching session.folder_path) must include proj_a.
        let res_a = tool
            .execute(json!({
                "changed_paths": ["crates/alpha/src/lib.rs"],
                "scope": "targeted",
                "cwd": repo_a.path().to_string_lossy(),
            }))
            .await
            .expect("verify repo_a");
        assert!(!res_a.is_error);

        let reqs = captured_requests.lock().await;
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].get("project_id").is_none() || reqs[0]["project_id"].is_null(),
            "expected project_id omitted when cwd != session.folder_path, got: {}",
            reqs[0]
        );
        assert_eq!(reqs[0]["repo_key"], expected_repo_b_key);
        assert_eq!(reqs[1]["project_id"], proj_a.to_string());
    }

    #[test]
    fn detect_local_full_commands_supports_vitest_jest_extensions_and_package_scripts() {
        for cfg in [
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mts",
            "vitest.config.mjs",
            "vitest.config.cts",
            "vitest.config.cjs",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), "{}").unwrap();
            std::fs::write(dir.path().join(cfg), "export default {}").unwrap();
            let cmds = detect_local_full_commands(dir.path());
            assert_eq!(cmds[0].command, "npx vitest run", "config {cfg}");
        }

        for cfg in [
            "jest.config.ts",
            "jest.config.js",
            "jest.config.mts",
            "jest.config.mjs",
            "jest.config.cts",
            "jest.config.cjs",
            "jest.config.json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), "{}").unwrap();
            std::fs::write(dir.path().join(cfg), "{}").unwrap();
            let cmds = detect_local_full_commands(dir.path());
            assert_eq!(cmds[0].command, "npx jest", "config {cfg}");
        }

        // package.json scripts mentioning vitest without config file
        let dir_vitest_script = tempfile::tempdir().unwrap();
        std::fs::write(
            dir_vitest_script.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_local_full_commands(dir_vitest_script.path())[0].command,
            "npx vitest run"
        );

        // Plain package.json falls back to npm test
        let dir_plain = tempfile::tempdir().unwrap();
        std::fs::write(
            dir_plain.path().join("package.json"),
            r#"{"scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_local_full_commands(dir_plain.path())[0].command,
            "npm test"
        );
    }
}
