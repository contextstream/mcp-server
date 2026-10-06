//! `contextstream-mcp link`: show, check and repair which ContextStream
//! project a local checkout is linked to.
//!
//! The same audit runs in the background in the sync bridge. There it only
//! repairs a link when the evidence is strong (the Git repository, or another
//! linked checkout of it on this machine), the target is not an isolated
//! benchmark/test workspace, and the folder is not pinned. Every change is
//! recorded with the previous link so it can be undone.

use anyhow::Result;
use mcp_client::ContextStreamClient;
use mcp_session::project_link::{
    assess, gather_checkout_facts, load_candidates, LinkAssessment, LinkStatus, ProjectCandidate,
    ScoredProject, Verdict,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Who asked for a repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairMode {
    /// Report only.
    Check,
    /// `link --fix`: apply any confident recommendation.
    Confident,
    /// Background bridge: apply only strongly evidenced recommendations.
    Automatic,
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkOutcome {
    pub assessment: LinkAssessment,
    pub pinned: bool,
    /// The project the folder was re-linked to, when a repair ran.
    pub relinked_to: Option<ScoredProject>,
    pub error: Option<String>,
}

fn contextstream_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream"))
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

fn read_mappings() -> Value {
    contextstream_dir()
        .map(|dir| read_json(&dir.join("mappings.json")))
        .unwrap_or(Value::Null)
}

// ============================================================================
// Pins and audit log
// ============================================================================

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pins {
    #[serde(default)]
    folders: BTreeMap<String, String>,
}

fn pins_path() -> Option<PathBuf> {
    contextstream_dir().map(|dir| dir.join("link-pins.json"))
}

fn read_pins() -> Pins {
    pins_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Keep the background audit from changing this folder's link.
pub fn pin_folder(root: &Path, project_id: Uuid) -> Result<()> {
    let path = pins_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let mut pins = read_pins();
    pins.folders
        .insert(root.to_string_lossy().to_string(), project_id.to_string());
    write_json_file(&path, &pins)
}

#[derive(Debug, Serialize, Deserialize)]
struct AuditEntry {
    at: String,
    folder: String,
    actor: String,
    previous_project_id: Option<String>,
    previous_workspace_id: Option<String>,
    project_id: String,
    workspace_id: String,
    reason: String,
}

const MAX_AUDIT_ENTRIES: usize = 500;

fn record_audit(entry: AuditEntry) {
    let Some(path) = contextstream_dir().map(|dir| dir.join("link-audit.json")) else {
        return;
    };
    let mut entries: Vec<AuditEntry> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    entries.push(entry);
    if entries.len() > MAX_AUDIT_ENTRIES {
        let excess = entries.len() - MAX_AUDIT_ENTRIES;
        entries.drain(..excess);
    }
    let _ = write_json_file(&path, &entries);
}

// ============================================================================
// Audit and repair
// ============================================================================

fn status_reason(status: &LinkStatus) -> String {
    match status {
        LinkStatus::Correct => "link is correct".to_string(),
        LinkStatus::Unverified => {
            "link has no verified repository fingerprint; re-established".to_string()
        }
        LinkStatus::Unlinked => "folder was not linked".to_string(),
        LinkStatus::Broken { reason } | LinkStatus::Wrong { reason } => reason.clone(),
    }
}

/// Link `root` to `target` with a verified repository fingerprint.
async fn relink(root: &Path, target: &ScoredProject) -> Result<()> {
    mcp_session::auto_init::establish_folder_binding(
        &root.to_string_lossy(),
        target.workspace_id,
        &target.workspace_name,
        Some(target.project_id),
        Some(&target.project_name),
    )
    .await
    .map_err(|error| anyhow::anyhow!("could not link {}: {error}", root.display()))?;
    Ok(())
}

/// Assess every folder and repair according to `mode`. `explicit` forces a
/// specific project (validated against the projects the account can open).
pub async fn audit_folders(
    candidates: &[ProjectCandidate],
    folders: &[PathBuf],
    mode: RepairMode,
    explicit: Option<Uuid>,
    actor: &str,
) -> Vec<LinkOutcome> {
    let mappings = read_mappings();
    let pins = read_pins();
    let now = chrono::Utc::now();
    let mut outcomes = Vec::new();
    for folder in folders {
        let facts = gather_checkout_facts(folder, &mappings);
        let pinned = pins
            .folders
            .contains_key(facts.root.to_string_lossy().as_ref());
        let assessment = assess(&facts, candidates, now);
        let target = match explicit {
            Some(project_id) => candidates
                .iter()
                .find(|candidate| candidate.project.id == project_id)
                .map(|candidate| {
                    assessment
                        .ranked
                        .iter()
                        .find(|scored| scored.project_id == project_id)
                        .cloned()
                        .unwrap_or_else(|| ScoredProject {
                            project_id,
                            project_name: candidate.project.name.clone(),
                            workspace_id: candidate.workspace.id,
                            workspace_name: candidate.workspace.name.clone(),
                            score: 0,
                            reasons: vec!["chosen explicitly".to_string()],
                            strong_evidence: false,
                            isolated_workspace: false,
                        })
                }),
            None => match mode {
                RepairMode::Check => None,
                RepairMode::Confident => assessment.recommended().cloned(),
                RepairMode::Automatic if !pinned && assessment.safe_to_auto_fix() => {
                    assessment.recommended().cloned()
                }
                RepairMode::Automatic => None,
            },
        };
        let mut outcome = LinkOutcome {
            assessment,
            pinned,
            relinked_to: None,
            error: None,
        };
        if explicit.is_some() && target.is_none() {
            outcome.error =
                Some("that project is not in any workspace this account can open".to_string());
        }
        if let Some(target) = target {
            let previous = outcome.assessment.current.clone();
            let unchanged = previous
                .as_ref()
                .is_some_and(|current| current.project_id == target.project_id && current.verified);
            if !unchanged {
                match relink(&facts.root, &target).await {
                    Ok(()) => {
                        record_audit(AuditEntry {
                            at: chrono::Utc::now().to_rfc3339(),
                            folder: facts.root.to_string_lossy().to_string(),
                            actor: actor.to_string(),
                            previous_project_id: previous
                                .as_ref()
                                .map(|current| current.project_id.to_string()),
                            previous_workspace_id: previous
                                .as_ref()
                                .and_then(|current| current.workspace_id)
                                .map(|id| id.to_string()),
                            project_id: target.project_id.to_string(),
                            workspace_id: target.workspace_id.to_string(),
                            reason: status_reason(&outcome.assessment.status),
                        });
                        outcome.relinked_to = Some(target);
                    }
                    Err(error) => outcome.error = Some(error.to_string()),
                }
            }
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Folders this machine has linked or indexed: every existing directory in
/// `mappings.json` and `indexed-projects.json`, reduced to checkout roots.
pub fn known_folders() -> Vec<PathBuf> {
    let Some(dir) = contextstream_dir() else {
        return Vec::new();
    };
    let mut paths: Vec<String> = Vec::new();
    let mappings = read_json(&dir.join("mappings.json"));
    if let Some(entries) = mappings
        .get("mappings")
        .and_then(Value::as_array)
        .or_else(|| mappings.as_array())
    {
        paths.extend(
            entries
                .iter()
                .filter_map(|entry| entry.get("path").and_then(Value::as_str))
                .map(str::to_string),
        );
    }
    let indexed = read_json(&dir.join("indexed-projects.json"));
    if let Some(projects) = indexed.get("projects").and_then(Value::as_object) {
        paths.extend(projects.keys().cloned());
    }
    let mut roots: Vec<PathBuf> = paths
        .into_iter()
        // Throwaway clones and worktrees under temporary directories are never
        // audited: linking them would start syncing scratch copies.
        .filter(|path| {
            Path::new(path).is_dir() && !mcp_session::project_link::is_generated_path(path)
        })
        .filter_map(|path| {
            mcp_session::checkout_repository_layout(&path)
                .map(|layout| layout.repository_root)
                .or_else(|| std::fs::canonicalize(&path).ok())
        })
        .filter(|root| {
            root.parent().is_some() && Some(root.as_path()) != dirs::home_dir().as_deref()
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Background audit for the sync bridge. Returns how many folders it
/// re-linked; failures are logged and never stop the bridge.
pub async fn background_audit(client: &ContextStreamClient) -> usize {
    let folders = known_folders();
    if folders.is_empty() {
        return 0;
    }
    let candidates = match load_candidates(client).await {
        Ok(candidates) if !candidates.is_empty() => candidates,
        Ok(_) => return 0,
        Err(error) => {
            tracing::debug!(%error, "link audit: could not list projects");
            return 0;
        }
    };
    let outcomes = audit_folders(
        &candidates,
        &folders,
        RepairMode::Automatic,
        None,
        "sync_bridge",
    )
    .await;
    let mut relinked = 0;
    for outcome in &outcomes {
        if let Some(target) = &outcome.relinked_to {
            relinked += 1;
            tracing::info!(
                folder = %outcome.assessment.root.display(),
                project = %target.project_name,
                workspace = %target.workspace_name,
                "link audit: re-linked folder"
            );
        } else if let Some(error) = &outcome.error {
            tracing::warn!(folder = %outcome.assessment.root.display(), %error, "link audit: repair failed");
        }
    }
    relinked
}

// ============================================================================
// CLI
// ============================================================================

pub struct LinkCommand {
    pub path: Option<PathBuf>,
    pub all: bool,
    pub fix: bool,
    pub project_id: Option<Uuid>,
    pub pin: bool,
    pub json: bool,
}

fn describe_status(status: &LinkStatus) -> (&'static str, String) {
    match status {
        LinkStatus::Correct => ("ok", "linked correctly".to_string()),
        LinkStatus::Unverified => (
            "warn",
            "linked to the right project, but without a verified repository fingerprint"
                .to_string(),
        ),
        LinkStatus::Unlinked => ("warn", "not linked".to_string()),
        LinkStatus::Broken { reason } => ("fail", reason.clone()),
        LinkStatus::Wrong { reason } => ("fail", reason.clone()),
    }
}

fn print_outcome(outcome: &LinkOutcome) {
    let assessment = &outcome.assessment;
    let (level, detail) = describe_status(&assessment.status);
    let mark = match level {
        "ok" => "✓",
        "warn" => "!",
        _ => "✗",
    };
    println!("{mark} {}", assessment.root.display());
    if let Some(repository) = &assessment.repository {
        println!("    repository  {repository}");
    }
    println!("    status      {detail}");
    if outcome.pinned {
        println!("    pinned      the background audit leaves this folder alone");
    }
    match &assessment.verdict {
        Verdict::Confident { best } => println!(
            "    best match  {} in {} (score {}: {})",
            best.project_name,
            best.workspace_name,
            best.score,
            best.reasons.join("; ")
        ),
        Verdict::Ambiguous { candidates } => {
            println!("    ambiguous   several projects fit; pick one with --project-id:");
            for candidate in candidates {
                println!(
                    "                {} {} in {} (score {})",
                    candidate.project_id,
                    candidate.project_name,
                    candidate.workspace_name,
                    candidate.score
                );
            }
        }
        Verdict::NoMatch => println!("    best match  none; link it with --project-id <id>"),
    }
    if let Some(target) = &outcome.relinked_to {
        println!(
            "    re-linked   {} in {} ({})",
            target.project_name, target.workspace_name, target.project_id
        );
    } else if let Some(error) = &outcome.error {
        println!("    error       {error}");
    }
}

/// Run `contextstream-mcp link`. Returns whether every folder ended up
/// correctly linked.
pub async fn run(command: LinkCommand) -> Result<bool> {
    let config = crate::config::load_config().map_err(|error| {
        anyhow::anyhow!(
            "no ContextStream credentials ({error}); run `contextstream-mcp setup` first"
        )
    })?;
    let client = ContextStreamClient::new(config);
    let folders = if command.all {
        known_folders()
    } else {
        let path = match command.path {
            Some(path) => path,
            None => std::env::current_dir()?,
        };
        vec![path]
    };
    let candidates = load_candidates(&client).await?;
    let mode = if command.fix || command.project_id.is_some() {
        RepairMode::Confident
    } else {
        RepairMode::Check
    };
    let outcomes = audit_folders(&candidates, &folders, mode, command.project_id, "cli").await;

    if command.pin {
        for outcome in &outcomes {
            let project = outcome
                .relinked_to
                .as_ref()
                .map(|target| target.project_id)
                .or(outcome
                    .assessment
                    .current
                    .as_ref()
                    .map(|current| current.project_id));
            if let Some(project) = project {
                pin_folder(&outcome.assessment.root, project)?;
            }
        }
    }

    if command.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        for outcome in &outcomes {
            print_outcome(outcome);
        }
        if mode == RepairMode::Check
            && outcomes
                .iter()
                .any(|outcome| outcome.assessment.recommended().is_some())
        {
            println!("\nRun again with --fix to apply the best matches.");
        }
    }
    if outcomes.iter().any(|outcome| outcome.relinked_to.is_some()) {
        let _ = crate::watch::request_sync_bridge_reload();
    }
    Ok(outcomes.iter().all(|outcome| {
        outcome.relinked_to.is_some() || matches!(outcome.assessment.status, LinkStatus::Correct)
    }))
}
