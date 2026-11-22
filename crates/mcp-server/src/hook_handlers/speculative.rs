//! Debounced speculative background checks on file edits (`hook hosted-speculative`).
//!
//! `maybe_spawn` checks local opt-in, records an edit generation under a file lock,
//! and spawns a detached `contextstream-mcp hook hosted-speculative` process group.
//! After `debounce_ms` (default 3000ms), the detached worker aborts cleanly if a
//! newer edit superseded `edit_ns` or if `target_tree == last_checked_tree`, and
//! otherwise runs up to 3 lint/typecheck/impacted-unit-test commands with
//! `speculative: true` under an isolated local hourly credit cap.

use anyhow::Result;
use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::hosted_tools::{PlanCheckRequest, PlanCheckResponse};
use mcp_client::{command_hints, ContextStreamClient};
use mcp_session::SessionManager;
use mcp_tools::domains::hosted_tools::{
    command_hints_for_run, local_speculative_override, prepare_repo_key,
    run_speculative_check_with_snapshot, SpeculativeCallStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{common, prompt_state};

pub(crate) const SPECULATIVE_HOOK: &str = "hosted-speculative";
const DEFAULT_DEBOUNCE_MS: u64 = 3000;
const DEBOUNCE_ENV: &str = "CONTEXTSTREAM_SPECULATIVE_DEBOUNCE_MS";
const GLOBAL_HOURLY_CAP_ENV: &str = "CONTEXTSTREAM_SPECULATIVE_HOURLY_CAP";
const REPO_HOURLY_CAP_ENV: &str = "CONTEXTSTREAM_SPECULATIVE_REPO_HOURLY_CAP";
const DEFAULT_GLOBAL_HOURLY_CAP: u32 = 30;
const DEFAULT_REPO_HOURLY_CAP: u32 = 15;
const MAX_SPECULATIVE_COMMANDS: usize = 3;
const DEFAULT_COOLDOWN_SECS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SpeculativeJobPayload {
    pub cwd: String,
    pub edit_ns: u64,
    #[serde(default)]
    pub debounce_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SpeculativeCreditState {
    pub hour_epoch: u64,
    pub global_used: u32,
    #[serde(default)]
    pub per_repo_used: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_epoch_s: Option<u64>,
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
}

fn is_noise_path(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    norm.contains("/.git/")
        || norm.ends_with("/.git")
        || norm.contains("/.contextstream/")
        || norm.ends_with("/.contextstream")
}

/// Precedence:
/// - `CONTEXTSTREAM_HOSTED_TOOLS=off` -> `false`
/// - `CONTEXTSTREAM_SPECULATIVE_CHECKS=0/false/no/off` -> `false` (hard override)
/// - `CONTEXTSTREAM_SPECULATIVE_CHECKS=1/true/yes/on` -> `true` (when config + `check` tool available)
/// - Unset / unparseable -> `command_hints::Cache.speculative_checks == Some(true)` (default `false`)
pub(crate) fn speculative_enabled(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return false;
    }
    let config = common::load_config(cwd);
    if !config.is_configured() {
        return false;
    }
    let identity = command_hints::local_identity(&config.api_key);
    may_schedule(
        local_speculative_override(),
        command_hints::read(&identity, &config.api_url).as_ref(),
    )
}

// A local opt-in may need an initial catalog read. The debounced worker confirms that
// the current caller actually has the check tool before it takes or sends a snapshot.
fn may_schedule(choice: Option<bool>, cache: Option<&command_hints::Cache>) -> bool {
    if cache.is_some_and(|c| {
        c.is_fresh(mcp_client::hosted_tools::KNOWN_TTL) && c.tool_named("check").is_none()
    }) {
        return false;
    }
    choice.unwrap_or_else(|| {
        cache.is_some_and(|c| c.speculative_checks == Some(true) && c.tool_named("check").is_some())
    })
}

/// Record edit timestamp and spawn a detached debounced speculative runner.
pub(crate) fn maybe_spawn(cwd: &str, edited_paths: &[String]) {
    if cfg!(test) || !speculative_enabled(cwd) {
        return;
    }
    if !edited_paths.iter().any(|p| !is_noise_path(p)) {
        return;
    }
    let config = common::load_config(cwd);
    let identity = command_hints::local_identity(&config.api_key);
    let Some(state_path) =
        command_hints::speculative_path("edit", &[&identity, &config.api_url, cwd])
    else {
        return;
    };
    let edit_ns = now_unix_nanos();
    if prompt_state::record_edit(&state_path, edit_ns).is_err() {
        return;
    }
    let debounce_ms = env_u64(DEBOUNCE_ENV).unwrap_or(DEFAULT_DEBOUNCE_MS);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .args(["hook", SPECULATIVE_HOOK])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let payload = SpeculativeJobPayload {
        cwd: cwd.to_string(),
        edit_ns,
        debounce_ms: Some(debounce_ms),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = serde_json::to_writer(&mut stdin, &payload);
    }
}

pub(crate) fn reserve_speculative_slot_at(
    path: &Path,
    repo_key: &str,
    now_secs: u64,
    global_cap: u32,
    repo_cap: u32,
) -> Result<u32, &'static str> {
    if global_cap == 0 || repo_cap == 0 {
        return Err("local speculative cap is 0");
    }
    let current_hour = now_secs / 3600;
    command_hints::update_speculative_state(path, |state: &mut SpeculativeCreditState| {
        if state.hour_epoch != current_hour {
            state.hour_epoch = current_hour;
            state.global_used = 0;
            state.per_repo_used.clear();
        }
        if state
            .cooldown_until_epoch_s
            .is_some_and(|until| now_secs < until)
        {
            return Err("speculative cooldown active");
        }
        if state.global_used >= global_cap {
            return Err("global hourly speculative cap reached");
        }
        let repo_used = state.per_repo_used.get(repo_key).copied().unwrap_or(0);
        if repo_used >= repo_cap {
            return Err("repo hourly speculative cap reached");
        }
        let reserved = (global_cap - state.global_used)
            .min(repo_cap - repo_used)
            .min(i32::MAX as u32);
        state.global_used += reserved;
        state
            .per_repo_used
            .insert(repo_key.to_string(), repo_used + reserved);
        Ok(reserved)
    })
    .map_err(|_| "failed to update speculative credit state")?
}

fn refund(path: &Path, repo: &str, at: u64, amount: u32) {
    let _ = command_hints::update_speculative_state(path, |s: &mut SpeculativeCreditState| {
        if s.hour_epoch == at / 3600 {
            if let Some(used) = s.per_repo_used.get_mut(repo) {
                let refund = amount.min(*used);
                *used -= refund;
                s.global_used = s.global_used.saturating_sub(refund);
            }
        }
    });
}

pub(crate) fn record_speculative_cooldown_at(path: &Path, now_secs: u64, retry_after_secs: u64) {
    let next_hour = (now_secs / 3600 + 1) * 3600;
    let until = now_secs
        .saturating_add(retry_after_secs.max(DEFAULT_COOLDOWN_SECS))
        .min(next_hour);
    let _ = command_hints::update_speculative_state(path, |state: &mut SpeculativeCreditState| {
        state.cooldown_until_epoch_s = Some(
            state
                .cooldown_until_epoch_s
                .map_or(until, |existing| existing.max(until)),
        );
    });
}

fn rel_paths_from_repo(repo_root: &Path, edited_paths: &[String]) -> Vec<String> {
    let canon_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    edited_paths
        .iter()
        .filter_map(|raw| {
            let p = Path::new(raw);
            let rel = if p.is_absolute() {
                p.strip_prefix(&canon_root)
                    .or_else(|_| p.strip_prefix(repo_root))
                    .ok()?
            } else {
                p
            };
            Some(rel.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

fn impacted_rust_crate(repo_root: &Path, rel_paths: &[String]) -> Option<String> {
    for rel in rel_paths {
        let mut cur = repo_root.join(rel);
        if cur.is_file() {
            cur.pop();
        }
        while cur.starts_with(repo_root) && cur != repo_root {
            if let Ok(text) = std::fs::read_to_string(cur.join("Cargo.toml")) {
                if let Ok(doc) = text.parse::<toml_edit::DocumentMut>() {
                    if let Some(name) = doc
                        .get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                    {
                        if !name.is_empty()
                            && name
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                        {
                            return Some(name.to_owned());
                        }
                    }
                }
            }
            if !cur.pop() {
                break;
            }
        }
    }
    None
}

fn impacted_go_pkg(rel_paths: &[String]) -> Option<String> {
    for rel in rel_paths {
        if rel.ends_with(".go") {
            if let Some((dir, _)) = rel.rsplit_once('/') {
                if !dir.is_empty() {
                    return Some(format!("./{dir}/..."));
                }
            }
            return Some("./...".to_string());
        }
    }
    None
}

fn node_package_manager(dir: &Path) -> &'static str {
    if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    }
}

/// Select up to 3 (cwd_rel, command) checks covering recent explicit commands,
/// optional `plan-check` suggestions, and canonical lint/typecheck/impacted-test markers.
pub(crate) fn select_speculative_commands(
    repo_root: &Path,
    cwd_rel: &str,
    edited_paths: &[String],
    recent_commands: &[String],
    plan_commands: &[(String, String)],
    hints: Option<&command_hints::Hints>,
) -> Vec<(String, String)> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut push = |cmd_cwd: &str, cmd: &str| {
        let cmd = cmd.trim();
        let norm_cwd = cmd_cwd.trim().trim_end_matches('/');
        if Path::new(cmd_cwd).is_absolute()
            || norm_cwd.split('/').any(|c| c == "..")
            || !cmd
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ._/-=:".contains(&b))
        {
            return;
        }
        if cmd.is_empty() {
            return;
        }
        if let Some(hints) = hints {
            if !hints.matches(cmd) {
                return;
            }
        }
        let key = (norm_cwd.to_string(), cmd.to_string());
        if seen.insert(key.clone()) {
            candidates.push(key);
        }
    };

    for (cmd_cwd, cmd) in plan_commands {
        push(cmd_cwd, cmd);
    }

    // Fall back to checks supported by the repository and the caller’s catalog.
    let norm_cwd = cwd_rel.trim().trim_matches('/');
    let work_dir = if norm_cwd.is_empty() {
        repo_root.to_path_buf()
    } else {
        let joined = repo_root.join(norm_cwd);
        if joined.is_dir() {
            joined
        } else {
            repo_root.to_path_buf()
        }
    };
    let rel_edited = rel_paths_from_repo(repo_root, edited_paths);

    // Rust
    if work_dir.join("Cargo.toml").is_file() || repo_root.join("Cargo.toml").is_file() {
        let rust_cwd = if work_dir.join("Cargo.toml").is_file() {
            norm_cwd
        } else {
            ""
        };
        if let Some(pkg) = impacted_rust_crate(repo_root, &rel_edited) {
            push(rust_cwd, &format!("cargo clippy -p {pkg} --lib --tests"));
            push(rust_cwd, &format!("cargo check -p {pkg}"));
            push(rust_cwd, &format!("cargo test -p {pkg} --lib"));
        } else {
            push(rust_cwd, "cargo clippy --lib --tests");
            push(rust_cwd, "cargo check");
            push(rust_cwd, "cargo test --lib");
        }
    }

    // Node / TypeScript
    let pkg_dir = if work_dir.join("package.json").is_file() {
        Some((norm_cwd, work_dir.clone()))
    } else if repo_root.join("package.json").is_file() {
        Some(("", repo_root.to_path_buf()))
    } else {
        None
    };
    if let Some((node_cwd, dir)) = pkg_dir {
        if let Ok(raw) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                let pm = node_package_manager(&dir);
                let scripts = parsed.get("scripts").and_then(Value::as_object);
                if scripts.is_some_and(|s| s.contains_key("lint")) {
                    push(node_cwd, &format!("{pm} run lint"));
                }
                if scripts.is_some_and(|s| s.contains_key("typecheck")) {
                    push(node_cwd, &format!("{pm} run typecheck"));
                } else if scripts.is_some_and(|s| s.contains_key("type-check")) {
                    push(node_cwd, &format!("{pm} run type-check"));
                } else if dir.join("tsconfig.json").is_file() {
                    push(node_cwd, "npx tsc --noEmit");
                }
                if scripts.is_some_and(|s| s.contains_key("test")) {
                    push(node_cwd, &format!("{pm} test"));
                }
            }
        }
    }

    // Python
    let py_dir = if work_dir.join("pyproject.toml").is_file()
        || work_dir.join("pytest.ini").is_file()
    {
        Some((norm_cwd, work_dir.clone()))
    } else if repo_root.join("pyproject.toml").is_file() || repo_root.join("pytest.ini").is_file() {
        Some(("", repo_root.to_path_buf()))
    } else {
        None
    };
    if let Some((py_cwd, dir)) = py_dir {
        let pyproj = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
        if dir.join("ruff.toml").is_file()
            || dir.join(".ruff.toml").is_file()
            || pyproj.contains("ruff")
        {
            push(py_cwd, "ruff check .");
        }
        if dir.join("mypy.ini").is_file() || pyproj.contains("mypy") {
            push(py_cwd, "mypy .");
        }
        if dir.join("uv.lock").is_file() {
            push(py_cwd, "uv run pytest");
        } else {
            push(py_cwd, "pytest");
        }
    }

    // Go
    let go_dir = if work_dir.join("go.mod").is_file() {
        Some(norm_cwd)
    } else if repo_root.join("go.mod").is_file() {
        Some("")
    } else {
        None
    };
    if let Some(go_cwd) = go_dir {
        push(go_cwd, "go vet ./...");
        if let Some(pkg) = impacted_go_pkg(
            &rel_edited
                .iter()
                .filter_map(|p| {
                    Path::new(p)
                        .strip_prefix(go_cwd)
                        .ok()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .collect::<Vec<_>>(),
        ) {
            push(go_cwd, &format!("go test {pkg}"));
        } else {
            push(go_cwd, "go test ./...");
        }
    }

    candidates.sort_by_key(|(cwd, cmd)| {
        recent_commands
            .iter()
            .position(|h| *h == command_hints::command_digest(cwd, cmd))
            .unwrap_or(usize::MAX)
    });
    candidates.truncate(MAX_SPECULATIVE_COMMANDS);
    candidates
}

async fn fetch_plan_check_commands(
    client: &ContextStreamClient,
    repo_key: &str,
    changed_paths: &[String],
) -> Vec<(String, String)> {
    let config = client.config().await;
    let Some(workspace) = config.default_workspace_id else {
        return Vec::new();
    };
    let body = PlanCheckRequest {
        workspace_id: workspace.to_string(),
        project_id: config.default_project_id.map(|id| id.to_string()),
        repo_key: Some(repo_key.to_string()),
        changed_paths: changed_paths.iter().take(128).cloned().collect(),
        scope: Some("targeted".to_string()),
        ..Default::default()
    };
    let Ok(answer) = client
        .plan_check(&body, std::time::Duration::from_millis(800))
        .await
    else {
        return Vec::new();
    };
    if !answer.is_success() {
        return Vec::new();
    }
    let Ok(plan) = serde_json::from_value::<PlanCheckResponse>(answer.data) else {
        return Vec::new();
    };
    plan.commands
        .into_iter()
        .map(|item| (item.cwd.unwrap_or_default(), item.command))
        .collect()
}

pub(crate) async fn run_speculative_job_with(
    client: &ContextStreamClient,
    session: &SessionManager,
    state_path: &Path,
    credit_path: &Path,
    hints: Option<&command_hints::Hints>,
    payload: SpeculativeJobPayload,
) -> usize {
    let debounce_ms = payload.debounce_ms.unwrap_or(DEFAULT_DEBOUNCE_MS);
    if debounce_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
    }

    let current = || {
        std::fs::read(state_path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<prompt_state::SpeculativeEdit>(&raw).ok())
            .is_some_and(|s| s.last_edit_ns == payload.edit_ns)
    };
    if !current() {
        return 0;
    }
    let config = client.config().await;
    let identity = command_hints::local_identity(config.api_key.as_deref().unwrap_or_default());
    let cached;
    let hints = if let Some(hints) = hints {
        hints
    } else {
        let Some(catalog) = command_hints_for_run(
            client,
            session,
            &identity,
            std::time::Duration::from_secs(2),
        )
        .await
        else {
            return 0;
        };
        if !catalog.is_fresh(mcp_client::hosted_tools::KNOWN_TTL)
            || !may_schedule(local_speculative_override(), Some(&catalog))
        {
            return 0;
        }
        cached = catalog;
        let Some(tool) = cached.tool_named("check") else {
            return 0;
        };
        &tool.hints
    };
    let Ok(snapshot) = CheckoutSnapshot::capture(Path::new(&payload.cwd)).await else {
        return 0;
    };
    let repo_key = prepare_repo_key(&snapshot.repo_name, &snapshot.origin_sha256);
    let mut edited: Vec<_> = snapshot.overlay.iter().map(|e| e.path.clone()).collect();
    edited.extend(snapshot.deletes.iter().cloned());
    edited.retain(|p| !snapshot.withheld.iter().any(|w| w.path == *p));
    if edited.is_empty() {
        return 0;
    }
    let claimed = command_hints::update_speculative_state(
        state_path,
        |s: &mut prompt_state::SpeculativeEdit| {
            if s.last_edit_ns != payload.edit_ns
                || s.claimed
                || (s.repo_key == repo_key && s.last_checked_tree == snapshot.target_tree)
            {
                return false;
            }
            if s.repo_key != repo_key {
                s.last_checked_tree.clear();
            }
            s.repo_key.clone_from(&repo_key);
            s.target_tree.clone_from(&snapshot.target_tree);
            s.claimed = true;
            true
        },
    );
    if !matches!(claimed, Ok(true)) {
        return 0;
    }
    let recent: Vec<String> =
        command_hints::speculative_path("recent", &[&identity, &config.api_url, &repo_key])
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
    let plan_cmds = fetch_plan_check_commands(client, &repo_key, &edited).await;
    let commands = select_speculative_commands(
        &snapshot.root,
        &snapshot.folder,
        &edited,
        &recent,
        &plan_cmds,
        Some(hints),
    );
    if commands.is_empty() {
        return 0;
    }

    let global_cap = env_u32(GLOBAL_HOURLY_CAP_ENV).unwrap_or(DEFAULT_GLOBAL_HOURLY_CAP);
    let repo_cap = env_u32(REPO_HOURLY_CAP_ENV).unwrap_or(DEFAULT_REPO_HOURLY_CAP);
    let mut executed = 0usize;
    let expected = commands.len();
    for (cmd_cwd, cmd) in commands {
        if !current() {
            break;
        }
        let now_s = now_unix_secs();
        let Ok(reserved) =
            reserve_speculative_slot_at(credit_path, &repo_key, now_s, global_cap, repo_cap)
        else {
            break;
        };
        match run_speculative_check_with_snapshot(
            client, session, &snapshot, &cmd_cwd, &cmd, reserved,
        )
        .await
        {
            SpeculativeCallStatus::Completed { ran, exit, credits } => {
                refund(
                    credit_path,
                    &repo_key,
                    now_s,
                    reserved.saturating_sub(credits),
                );
                if ran && exit.is_some_and(|e| !matches!(e, 75 | 124 | 126 | 127 | 134 | 137)) {
                    executed += 1;
                }
            }
            SpeculativeCallStatus::RateLimited { .. } => {
                record_speculative_cooldown_at(credit_path, now_s, DEFAULT_COOLDOWN_SECS);
                refund(credit_path, &repo_key, now_s, reserved);
                break;
            }
            SpeculativeCallStatus::NotStarted => {
                refund(credit_path, &repo_key, now_s, reserved);
                break;
            }
            SpeculativeCallStatus::Failed => break, // keep the reservation if the stream outcome is unknown
        }
    }
    if executed == expected {
        let _ = command_hints::update_speculative_state(
            state_path,
            |s: &mut prompt_state::SpeculativeEdit| {
                if s.last_edit_ns == payload.edit_ns {
                    s.last_checked_tree.clone_from(&snapshot.target_tree);
                }
            },
        );
    }
    executed
}

/// Detached worker entrypoint for `contextstream-mcp hook hosted-speculative`.
pub(crate) async fn run_background_speculative() -> Result<()> {
    let raw = super::read_stdin_json().unwrap_or(Value::Null);
    let Ok(payload) = serde_json::from_value::<SpeculativeJobPayload>(raw) else {
        return Ok(());
    };
    if payload.cwd.trim().is_empty() || !speculative_enabled(&payload.cwd) {
        return Ok(());
    }
    let config = common::load_config(&payload.cwd);
    if !config.is_configured() {
        return Ok(());
    }
    let identity = command_hints::local_identity(&config.api_key);
    let Some(state_path) =
        command_hints::speculative_path("edit", &[&identity, &config.api_url, &payload.cwd])
    else {
        return Ok(());
    };
    let Some(credit_path) =
        command_hints::speculative_path("credits", &[&identity, &config.api_url])
    else {
        return Ok(());
    };
    let client_config = mcp_types::Config {
        api_url: config.api_url,
        api_key: Some(config.api_key),
        default_workspace_id: config
            .workspace_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        default_project_id: config
            .project_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(client_config.clone());
    let session = SessionManager::new(client.clone(), client_config);
    let _ =
        run_speculative_job_with(&client, &session, &state_path, &credit_path, None, payload).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_go_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/spec\n\ngo 1.24\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("go.sum"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/lib.go"), "package pkg\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "init",
            ],
        );
        std::fs::write(dir.path().join("pkg/lib.go"), "package pkg\n// edited\n").unwrap();
        dir
    }

    #[test]
    fn speculative_opt_in_requires_consent_and_a_current_tool() {
        let mut cache = command_hints::Cache::new("test", "https://example.invalid", vec![]);
        assert!(!may_schedule(None, None));
        assert!(may_schedule(Some(true), None)); // worker still has to fetch the catalog
        assert!(!may_schedule(Some(true), Some(&cache))); // known denied
        cache.tools.push(command_hints::CachedTool {
            name: "check".into(),
            transport: "stream".into(),
            attachments: vec![],
            hints: command_hints::Hints {
                argument: "command".into(),
                cwd_argument: None,
                patterns: vec![],
            },
        });
        assert!(!may_schedule(None, Some(&cache)));
        assert!(may_schedule(Some(true), Some(&cache)));
        cache.speculative_checks = Some(true);
        assert!(may_schedule(None, Some(&cache)));
        assert!(!may_schedule(Some(false), Some(&cache)));
    }

    #[test]
    fn command_selection_prefers_recent_explicit_and_impacted_markers_capped_at_three() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/a/src")).unwrap();
        std::fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let edited = vec![dir
            .path()
            .join("crates/a/src/lib.rs")
            .to_string_lossy()
            .to_string()];
        let hints = command_hints::Hints {
            argument: "command".into(),
            cwd_argument: Some("cwd".into()),
            patterns: vec!["^cargo (check|clippy|test)( .*)?$".into()],
        };
        let cmds = select_speculative_commands(
            dir.path(),
            "",
            &edited,
            &[command_hints::command_digest(
                "",
                "cargo test -p crate-a --lib",
            )],
            &[],
            Some(&hints),
        );
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].1, "cargo test -p crate-a --lib");
        assert!(cmds
            .iter()
            .any(|(_, c)| c == "cargo clippy -p crate-a --lib --tests"));
        assert!(cmds.iter().any(|(_, c)| c == "cargo check -p crate-a"));
    }

    #[test]
    fn local_hourly_credit_cap_and_cooldown_isolate_from_explicit_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let credit_path = tmp.path().join("speculative-credits.json");
        let t = 1_800_000_000_u64;
        assert_eq!(
            reserve_speculative_slot_at(&credit_path, "repo-1", t, 3, 2),
            Ok(2)
        );
        refund(&credit_path, "repo-1", t, 1);
        assert_eq!(
            reserve_speculative_slot_at(&credit_path, "repo-1", t, 3, 2),
            Ok(1)
        );
        // Repo cap 2 reached.
        assert!(reserve_speculative_slot_at(&credit_path, "repo-1", t, 3, 2).is_err());
        // Different repo uses remaining global slot (3rd).
        assert!(reserve_speculative_slot_at(&credit_path, "repo-2", t, 3, 2).is_ok());
        // Global cap 3 reached.
        assert!(reserve_speculative_slot_at(&credit_path, "repo-2", t, 3, 2).is_err());

        // Next hour resets counters.
        assert!(reserve_speculative_slot_at(&credit_path, "repo-1", t + 3600, 3, 2).is_ok());

        // Cooldown blocks subsequent speculative reservation within the window.
        record_speculative_cooldown_at(&credit_path, t + 3600, 300);
        assert!(reserve_speculative_slot_at(&credit_path, "repo-1", t + 3610, 3, 2).is_err());
    }

    #[tokio::test]
    #[ignore = "cross-repository acceptance run; requires a sibling contextstream checkout"]
    async fn debounced_runner_supersedes_burst_edits_dedups_tree_and_warms_real_mcd_cache() {
        // Locate contextstream/scripts/ops/microcompiler/mcd.py in the sibling worktree.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mcd_py = manifest_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("contextstream/scripts/ops/microcompiler/mcd.py");
        assert!(
            mcd_py.is_file(),
            "This integration test requires the Task 2.3 contextstream sibling checkout"
        );

        let repo = init_go_repo();
        let cwd = repo.path().to_string_lossy().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let mc_root = tmp.path().join("mcroot");
        let stub_bin = tmp.path().join("bin");
        std::fs::create_dir_all(&stub_bin).unwrap();
        let exec_log = tmp.path().join("mcd_exec.log");
        let go = which::which("go").expect("acceptance test requires Go on PATH");
        // Count actual invocations and keep dependency resolution offline. Use the installed
        // Go toolchain; the fixture has no third-party dependencies.
        let go = command_hints::shell_quote(go.to_str().unwrap());
        let log = command_hints::shell_quote(exec_log.to_str().unwrap());
        std::fs::write(stub_bin.join("go"), format!(
            "#!/bin/sh\ncase $1 in vet|test) echo ran >> {log};; esac\nexport GOPROXY=off GOSUMDB=off GOTOOLCHAIN=local\nexec {go} \"$@\"\n"
        )).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(stub_bin.join("go"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        // Copy core mcd files into tmp core dir stripping mc_env.py just like test_mcd_prewarm.py.
        let mcd_dir = mcd_py.parent().unwrap();
        let core_dir = tmp.path().join("mcd_core");
        std::fs::create_dir_all(&core_dir).unwrap();
        for entry in std::fs::read_dir(mcd_dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("py") {
                std::fs::copy(&path, core_dir.join(path.file_name().unwrap())).unwrap();
            }
        }
        let mcd_text = std::fs::read_to_string(core_dir.join("mcd.py")).unwrap();
        std::fs::write(
            core_dir.join("mcd.py"),
            mcd_text.replace(", \"mc_env.py\")", ")"),
        )
        .unwrap();
        let core_mcd = core_dir.join("mcd.py");

        // Start a real WebSocket bridge that spawns `python3 mcd.py serve` per connection.
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let mc_path_env = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let ws_server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = ws_listener.accept().await else {
                    break;
                };
                let core_mcd = core_mcd.clone();
                let mc_root = mc_root.clone();
                let mc_path_env = mc_path_env.clone();
                tokio::spawn(async move {
                    let ws = accept_async(stream).await.unwrap();
                    let mut child = tokio::process::Command::new("python3")
                        .arg(&core_mcd)
                        .arg("serve")
                        .env("MC_ROOT", &mc_root)
                        .env("MC_SCOPE", "0")
                        .env("MC_TOOLCHAIN_ID", "test")
                        .env("MC_GO_VERSION", "1.24.0")
                        .env("MC_FREE_MIN_GB", "0")
                        .env("MC_PATH", &mc_path_env)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    let mut child_in = child.stdin.take().unwrap();
                    let mut child_out = child.stdout.take().unwrap();

                    let (mut ws_sink, mut ws_stream) = ws.split();
                    let to_child = tokio::spawn(async move {
                        use tokio::io::AsyncWriteExt;
                        while let Some(Ok(msg)) = ws_stream.next().await {
                            if let Message::Binary(data) = msg {
                                if child_in.write_all(&data).await.is_err() {
                                    break;
                                }
                                let _ = child_in.flush().await;
                            }
                        }
                    });
                    let from_child = tokio::spawn(async move {
                        use tokio::io::AsyncReadExt;
                        let mut buf = [0u8; 65536];
                        loop {
                            match child_out.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if ws_sink
                                        .send(Message::binary(buf[..n].to_vec()))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    let _ = tokio::join!(to_child, from_child);
                    let _ = child.kill().await;
                });
            }
        });

        #[derive(Clone)]
        struct ApiState {
            ws_url: String,
            calls: Arc<Mutex<Vec<Value>>>,
            call_seq: Arc<AtomicUsize>,
        }
        let api_state = ApiState {
            ws_url: format!("ws://{ws_addr}/run"),
            calls: Arc::new(Mutex::new(Vec::new())),
            call_seq: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route(
                "/mcp/hosted-tools",
                get(|| async {
                    Json(json!({
                        "success": true,
                        "data": {
                            "tools": [{
                                "name": "check",
                                "description": "Remote check",
                                "input_schema": {"type": "object"},
                                "transport": {"kind": "stream"},
                                "attachments": ["checkout_snapshot"],
                                "command_hints": {
                                    "argument": "command",
                                    "cwd_argument": "cwd",
                                    "patterns": ["^go (vet|test)( .*)?$"],
                                    "speculative_checks": true
                                }
                            }],
                            "speculative_checks": true
                        }
                    }))
                }),
            )
            .route(
                "/mcp/hosted-tools/check/call",
                post(
                    |State(st): State<ApiState>, Json(body): Json<Value>| async move {
                        st.calls.lock().unwrap().push(body.clone());
                        let seq = st.call_seq.fetch_add(1, Ordering::SeqCst);
                        let cmd = body["arguments"]["command"].as_str().unwrap().to_string();
                        let cwd_arg = body["arguments"]["cwd"].as_str().unwrap_or("").to_string();
                        let speculative =
                            body["arguments"]["speculative"].as_bool().unwrap_or(false);
                        let mut req = json!({
                            "id": format!("mc-{seq:016x}"),
                            "command": cmd,
                            "cwd": cwd_arg,
                            "env_names": [],
                            "timeout_s": 60,
                            "wait_s": if speculative { 1 } else { 30 },
                            "pipefail": false,
                            "repo_key": "spec-e2e-12345678",
                            "tool_result": true,
                        });
                        if speculative {
                            req["speculative"] = json!(true);
                        }
                        (
                            StatusCode::OK,
                            Json(json!({
                                "success": true,
                                "data": {
                                    "stream": {
                                        "url": st.ws_url,
                                        "token": format!("tok-{seq}"),
                                        "protocol": mcp_client::frames::PROTOCOL,
                                        "request": req,
                                    },
                                    "call": { "id": format!("call-{seq}"), "speculative_credit_limit": body["arguments"]["speculative_credit_limit"], "credits_per_10s": 1 }
                                }
                            })),
                        )
                    },
                ),
            )
            .with_state(api_state.clone());

        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api_listener.local_addr().unwrap();
        let api_server = tokio::spawn(async move {
            axum::serve(api_listener, Router::new().nest("/api/v1", app))
                .await
                .unwrap();
        });

        let api_url = format!("http://{api_addr}");
        let client_cfg = mcp_types::Config {
            api_url,
            api_key: Some("cs_e2e_key".into()),
            ..Default::default()
        };
        let client = ContextStreamClient::new(client_cfg.clone());
        let session = SessionManager::new(client.clone(), client_cfg);
        session
            .initialize(None, None, Some(cwd.clone()), None)
            .await;

        let state_path = tmp.path().join("speculative-state.json");
        let credit_path = tmp.path().join("speculative-credits.json");
        let hints = command_hints::Hints {
            argument: "command".into(),
            cwd_argument: Some("cwd".into()),
            patterns: vec!["^go (vet|test)( .*)?$".into()],
        };

        std::fs::write(repo.path().join("pkg/lib.go"), "package pkg\n// saved v2\n").unwrap();
        let saved = tokio::time::Instant::now();
        // 1. Simulate burst edits: edit_ns=100 is superseded by edit_ns=200.
        prompt_state::record_edit(&state_path, 100).unwrap();
        prompt_state::record_edit(&state_path, 200).unwrap();

        let superseded = run_speculative_job_with(
            &client,
            &session,
            &state_path,
            &credit_path,
            Some(&hints),
            SpeculativeJobPayload {
                cwd: cwd.clone(),
                edit_ns: 100,
                debounce_ms: Some(10),
            },
        )
        .await;
        assert_eq!(superseded, 0, "superseded edit must exit without API calls");

        // Latest edit (edit_ns=200) runs speculative checks against real mcd.py.
        let background = run_speculative_job_with(
            &client,
            &session,
            &state_path,
            &credit_path,
            Some(&hints),
            SpeculativeJobPayload {
                cwd: cwd.clone(),
                edit_ns: 200,
                debounce_ms: Some(DEFAULT_DEBOUNCE_MS),
            },
        );
        let explicit_checks = async {
            // Wait 5s to match ">= 5s after save" contract before explicit repeat checks.
            tokio::time::sleep_until(saved + std::time::Duration::from_secs(5)).await;

            let gate = mcp_tools::domains::hosted_tools::HostedToolsGate::new();
            let tool = mcp_tools::domains::hosted_tools::find(&gate, &client, &session, "check")
                .await
                .unwrap();

            // Include an eligible, unpredicted command as a negative control.
            let explicit_cmds = ["go vet ./...", "go test ./pkg/...", "go test ./..."];
            let mut hits = 0;
            let mut samples = Vec::new();
            let worker_module = mcd_py
                .ancestors()
                .nth(4)
                .unwrap()
                .join("deploy/cloudflare/cloudflare-microcompiler/src/worker.ts");
            for cmd in explicit_cmds {
                let after_save = saved.elapsed();
                assert!(after_save >= std::time::Duration::from_secs(5));
                let start = std::time::Instant::now();
                let res = mcp_tools::domains::hosted_tools::run(
                    &client,
                    &session,
                    &tool,
                    json!({"command":cmd}),
                )
                .await;
                let elapsed = start.elapsed();
                assert!(!res.is_error, "explicit check failed");
                let frame = res.structured_content.as_ref().unwrap();
                let hit = frame["cache"] == "hit";
                assert_eq!(hit, cmd != "go test ./...");
                if hit {
                    hits += 1;
                    assert!(
                        elapsed < std::time::Duration::from_secs(1),
                        "cache hit took {elapsed:?}"
                    );
                }
                let usage=std::process::Command::new("node").args(["--disable-warning=ExperimentalWarning","--input-type=module","-e",
                "import {pathToFileURL} from 'node:url'; const {usageBody}=await import(pathToFileURL(process.argv[1])); console.log(usageBody({result:JSON.parse(process.argv[2])},null,'go',Number(process.argv[3]),0));"])
                .arg(&worker_module).arg(frame.to_string()).arg(elapsed.as_millis().to_string()).output().unwrap();
                assert!(usage.status.success(), "worker usage adapter failed");
                let usage: Value = serde_json::from_slice(&usage.stdout).unwrap();
                if hit {
                    assert_eq!(usage["server_ms"], 0);
                }
                eprintln!(
                    "{cmd} after_save={after_save:?} elapsed={elapsed:?} cache={} server_ms={}",
                    frame["cache"], usage["server_ms"]
                );
                samples.push(usage);
            }
            assert!(hits * 2 > explicit_cmds.len());
            eprintln!("usage_samples={}", serde_json::to_string(&samples).unwrap());
            // Two speculative executions and just the negative control on the explicit path.
            assert_eq!(
                std::fs::read_to_string(&exec_log).unwrap().lines().count(),
                3
            );
        };
        let (ran, ()) = tokio::join!(background, explicit_checks);

        assert_eq!(
            ran,
            2,
            "expected two warm commands; API calls={}",
            api_state.calls.lock().unwrap().len()
        );

        // Unchanged target_tree on edit_ns=300 skips redundant speculative run.
        prompt_state::record_edit(&state_path, 300).unwrap();
        let deduped = run_speculative_job_with(
            &client,
            &session,
            &state_path,
            &credit_path,
            Some(&hints),
            SpeculativeJobPayload {
                cwd: cwd.clone(),
                edit_ns: 300,
                debounce_ms: Some(10),
            },
        )
        .await;
        assert_eq!(deduped, 0, "unchanged target_tree must deduplicate");

        ws_server.abort();
        api_server.abort();
    }
}
