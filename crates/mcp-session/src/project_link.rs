//! Decide which ContextStream project a local checkout belongs to, and tell
//! when its current link is wrong.
//!
//! A checkout gets linked to the wrong project in a few recurring ways: a
//! benchmark or test harness ran setup in a real checkout, an old link points
//! at a workspace the account can no longer open, or a worktree or clone with
//! a dated folder name got a brand-new project named after the folder. The
//! resolver ranks every project the account can open with signals that do not
//! depend on the machine:
//!
//! - the repository the checkout tracks (its Git remote),
//! - another checkout of the same repository on this machine that is already
//!   linked (worktrees and clones share it),
//! - project, repository, folder and main-checkout names,
//! - where the project has been indexed before and how active it is,
//!
//! and it penalizes benchmark/isolated/test workspaces and generated,
//! worktree-named or temporary projects. Only a clear winner is "confident".

use mcp_types::api::{Project, Workspace};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::checkout_identity::{
    checkout_repository_layout, current_repository_fingerprint, current_repository_remote_identity,
    RepositoryRemoteIdentity,
};

/// Lowest score a project needs to be chosen.
pub const CONFIDENT_MIN_SCORE: i32 = 60;
/// Lead the best project needs over the runner-up to be chosen.
pub const CONFIDENT_MARGIN: i32 = 25;

const SCORE_REPOSITORY: i32 = 100;
const SCORE_SIBLING_LINK: i32 = 60;
const SCORE_REPOSITORY_NAME: i32 = 40;
const SCORE_FOLDER_NAME: i32 = 30;
const SCORE_WORKTREE_PREFIX: i32 = 10;
const SCORE_KNOWN_PATH: i32 = 15;
const SCORE_INDEXED: i32 = 10;
const SCORE_RECENT: i32 = 5;
const SCORE_TEAM: i32 = 5;
/// The folder's existing link is history: it breaks near-ties between
/// duplicates, but never props up an isolated or generated project.
const SCORE_CURRENT_LINK: i32 = 20;
const PENALTY_ISOLATED_WORKSPACE: i32 = -80;
const PENALTY_GENERATED_PROJECT: i32 = -50;
const RECENT_DAYS: i64 = 30;

/// The link a checkout currently has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentLink {
    pub workspace_id: Option<Uuid>,
    pub project_id: Uuid,
    /// The link carries a repository fingerprint that still matches.
    pub verified: bool,
    /// `checkout_config` or `folder_mapping`.
    pub source: &'static str,
}

/// What can be learned about a checkout without the network.
#[derive(Clone, Debug, Default)]
pub struct CheckoutFacts {
    pub root: PathBuf,
    pub folder_name: String,
    pub repository: Option<RepositoryRemoteIdentity>,
    pub repository_name: Option<String>,
    pub main_checkout_name: Option<String>,
    /// Projects that other verified checkouts of the same repository on
    /// this machine are linked to.
    pub sibling_projects: Vec<Uuid>,
    pub current: Option<CurrentLink>,
}

/// One project the account can open, with its workspace.
#[derive(Clone, Debug)]
pub struct ProjectCandidate {
    pub project: Project,
    pub workspace: Workspace,
}

/// A candidate's score and the evidence behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScoredProject {
    pub project_id: Uuid,
    pub project_name: String,
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub score: i32,
    pub reasons: Vec<String>,
    /// Repository or sibling-checkout evidence, not just names.
    pub strong_evidence: bool,
    pub isolated_workspace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Confident { best: ScoredProject },
    Ambiguous { candidates: Vec<ScoredProject> },
    NoMatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LinkStatus {
    Correct,
    /// Points at the right project but without a verified repository
    /// fingerprint, so automatic content writers ignore it.
    Unverified,
    Unlinked,
    Broken {
        reason: String,
    },
    Wrong {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkAssessment {
    pub root: PathBuf,
    pub repository: Option<String>,
    pub current: Option<CurrentLink>,
    pub status: LinkStatus,
    pub verdict: Verdict,
    /// Top candidates, best first.
    pub ranked: Vec<ScoredProject>,
}

impl LinkAssessment {
    /// The project to link to when the link should change.
    pub fn recommended(&self) -> Option<&ScoredProject> {
        match (&self.status, &self.verdict) {
            (LinkStatus::Correct, _) => None,
            (_, Verdict::Confident { best }) => Some(best),
            _ => None,
        }
    }

    /// Whether an unattended background repair may apply the
    /// recommendation: a confident, strongly evidenced, non-isolated
    /// project, and a current link that is missing, broken or wrong.
    pub fn safe_to_auto_fix(&self) -> bool {
        let Some(best) = self.recommended() else {
            return false;
        };
        best.strong_evidence
            && !best.isolated_workspace
            && matches!(
                self.status,
                LinkStatus::Unlinked
                    | LinkStatus::Unverified
                    | LinkStatus::Broken { .. }
                    | LinkStatus::Wrong { .. }
            )
    }
}

// ============================================================================
// Name and workspace heuristics
// ============================================================================

fn normalize_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c == '_' || c == '.' || c == ' ' {
                '-'
            } else {
                c
            }
        })
        .collect()
}

fn last_path_segment(path: &str) -> Option<String> {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .map(normalize_name)
        .filter(|segment| !segment.is_empty())
}

/// Workspaces that hold benchmark corpora, test fixtures or disposable data.
pub fn is_isolated_workspace(workspace: &Workspace) -> bool {
    const MARKERS: &[&str] = &[
        "benchmark",
        "isolated",
        "isolation",
        "disposable",
        "fixture",
        "harness",
        "longmemeval",
        "test data",
        "safe to wipe",
    ];
    let text = format!(
        "{} {}",
        workspace.name,
        workspace.description.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    MARKERS.iter().any(|marker| text.contains(marker))
}

fn has_long_digit_run(text: &str, length: usize) -> bool {
    let mut run = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run >= length {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Names and paths that look generated: onboarding `project-<hex>`, dated
/// worktree or QA folders, and anything under a temporary directory.
pub fn is_generated_name(name: &str) -> bool {
    let name = normalize_name(name);
    if let Some(suffix) = name.strip_prefix("project-") {
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
    }
    const MARKERS: &[&str] = &[
        "retest", "cslive", "worktree", "scratch", "-tmp", "tmp-", "reseed", "-wt-", "wt-",
    ];
    // A trailing all-digit segment (`cs-release-07229`) marks a build or
    // release checkout rather than a project.
    let numbered_suffix = name.rsplit('-').next().is_some_and(|last| {
        last.len() >= 4 && last.len() < name.len() && last.chars().all(|c| c.is_ascii_digit())
    });
    has_long_digit_run(&name, 6)
        || numbered_suffix
        || MARKERS.iter().any(|marker| name.contains(marker))
}

/// Paths under temporary directories or with generated folder names.
pub fn is_generated_path(path: &str) -> bool {
    const MARKERS: &[&str] = &[
        "/tmp/",
        "/private/tmp/",
        "/var/folders/",
        "/scratchpad/",
        "/own-actions-runs/",
        "/dev/shm/",
    ];
    let path = path.replace('\\', "/");
    MARKERS.iter().any(|marker| path.contains(marker))
        || last_path_segment(&path).is_some_and(|segment| is_generated_name(&segment))
}

fn is_recent(timestamp: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    timestamp
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|at| now.signed_duration_since(at) <= chrono::Duration::days(RECENT_DAYS))
}

// ============================================================================
// Scoring
// ============================================================================

/// Score every candidate for `facts`, best first.
pub fn score_candidates(
    facts: &CheckoutFacts,
    candidates: &[ProjectCandidate],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<ScoredProject> {
    let folder = normalize_name(&facts.folder_name);
    let folder_generated = is_generated_name(&folder);
    let repository_name = facts.repository_name.as_deref().map(normalize_name);
    let main_name = facts.main_checkout_name.as_deref().map(normalize_name);
    let siblings: HashSet<Uuid> = facts.sibling_projects.iter().copied().collect();
    let current_project = facts.current.as_ref().map(|current| current.project_id);

    let mut scored: Vec<ScoredProject> = candidates
        .iter()
        .map(|candidate| {
            let project = &candidate.project;
            let name = normalize_name(&project.name);
            let mut score = 0;
            let mut reasons = Vec::new();
            let mut strong = false;

            let project_repository = project
                .repository_url
                .as_deref()
                .and_then(|url| RepositoryRemoteIdentity::from_remote_url(url).ok());
            if let (Some(ours), Some(theirs)) = (&facts.repository, &project_repository) {
                if ours == theirs {
                    score += SCORE_REPOSITORY;
                    strong = true;
                    reasons.push("tracks the same Git repository".to_string());
                }
            }
            if siblings.contains(&project.id) {
                score += SCORE_SIBLING_LINK;
                strong = true;
                reasons.push(
                    "another checkout of this repository on this machine is linked here"
                        .to_string(),
                );
            }
            if repository_name.as_deref() == Some(name.as_str()) {
                score += SCORE_REPOSITORY_NAME;
                reasons.push("name matches the repository name".to_string());
            } else if main_name.as_deref() == Some(name.as_str())
                || (!folder_generated && name == folder)
            {
                score += SCORE_FOLDER_NAME;
                reasons.push("name matches the checkout folder".to_string());
            } else if !name.is_empty()
                && (folder.starts_with(&format!("{name}-"))
                    || folder.starts_with(&format!("{name}_")))
            {
                score += SCORE_WORKTREE_PREFIX;
                reasons.push("checkout folder starts with the project name".to_string());
            }
            if let Some(segment) = project.path.as_deref().and_then(last_path_segment) {
                if repository_name.as_deref() == Some(segment.as_str())
                    || main_name.as_deref() == Some(segment.as_str())
                    || (!folder_generated && segment == folder)
                {
                    score += SCORE_KNOWN_PATH;
                    reasons.push("indexed from a folder of the same name before".to_string());
                }
            }
            if project.file_count.unwrap_or(0) > 0 {
                score += SCORE_INDEXED;
                reasons.push("has indexed files".to_string());
            }
            if is_recent(project.indexed_at.as_deref(), now)
                || is_recent(project.updated_at.as_deref(), now)
            {
                score += SCORE_RECENT;
                reasons.push("active in the last 30 days".to_string());
            }
            if candidate.workspace.visibility.as_deref() == Some("team") {
                score += SCORE_TEAM;
                reasons.push("team workspace".to_string());
            }
            let isolated = is_isolated_workspace(&candidate.workspace);
            if isolated {
                score += PENALTY_ISOLATED_WORKSPACE;
                reasons.push("workspace is for benchmarks, fixtures or test data".to_string());
            }
            let generated = is_generated_name(&project.name)
                || project.path.as_deref().is_some_and(is_generated_path);
            if generated {
                score += PENALTY_GENERATED_PROJECT;
                reasons.push(
                    "looks generated (worktree, dated, temporary or onboarding name)".to_string(),
                );
            }
            if current_project == Some(project.id) && !isolated && !generated {
                score += SCORE_CURRENT_LINK;
                reasons.push("this folder is already linked here".to_string());
            }

            ScoredProject {
                project_id: project.id,
                project_name: project.name.clone(),
                workspace_id: candidate.workspace.id,
                workspace_name: candidate.workspace.name.clone(),
                score,
                reasons,
                strong_evidence: strong,
                isolated_workspace: isolated,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.project_name.cmp(&b.project_name))
    });
    scored
}

/// A clear winner, a close call, or nothing plausible.
pub fn verdict(ranked: &[ScoredProject]) -> Verdict {
    let Some(best) = ranked.first() else {
        return Verdict::NoMatch;
    };
    if best.score < CONFIDENT_MIN_SCORE {
        return Verdict::NoMatch;
    }
    let runner_up = ranked.get(1).map(|second| second.score).unwrap_or(i32::MIN);
    if best.score.saturating_sub(runner_up) >= CONFIDENT_MARGIN {
        Verdict::Confident { best: best.clone() }
    } else {
        Verdict::Ambiguous {
            candidates: ranked
                .iter()
                .take_while(|candidate| best.score - candidate.score < CONFIDENT_MARGIN)
                .take(5)
                .cloned()
                .collect(),
        }
    }
}

/// Judge the checkout's current link against the ranked candidates.
pub fn assess(
    facts: &CheckoutFacts,
    candidates: &[ProjectCandidate],
    now: chrono::DateTime<chrono::Utc>,
) -> LinkAssessment {
    let ranked = score_candidates(facts, candidates, now);
    let verdict = verdict(&ranked);
    let status = match &facts.current {
        None => LinkStatus::Unlinked,
        Some(current) => match ranked.iter().find(|p| p.project_id == current.project_id) {
            None => LinkStatus::Broken {
                reason: format!(
                    "linked project {} is not in any workspace this account can open",
                    current.project_id
                ),
            },
            Some(linked) => match &verdict {
                Verdict::Confident { best } if best.project_id != linked.project_id => {
                    if linked.isolated_workspace && !best.isolated_workspace {
                        LinkStatus::Wrong {
                            reason: format!(
                                "linked to '{}' in the isolated workspace '{}'; '{}' in '{}' matches this checkout",
                                linked.project_name,
                                linked.workspace_name,
                                best.project_name,
                                best.workspace_name
                            ),
                        }
                    } else if best.score - linked.score >= CONFIDENT_MARGIN {
                        LinkStatus::Wrong {
                            reason: format!(
                                "linked to '{}' ({}), but '{}' in '{}' matches this checkout far better",
                                linked.project_name,
                                linked.workspace_name,
                                best.project_name,
                                best.workspace_name
                            ),
                        }
                    } else if current.verified {
                        LinkStatus::Correct
                    } else {
                        LinkStatus::Unverified
                    }
                }
                _ if current.verified => LinkStatus::Correct,
                _ => LinkStatus::Unverified,
            },
        },
    };
    LinkAssessment {
        root: facts.root.clone(),
        repository: facts
            .repository
            .as_ref()
            .map(RepositoryRemoteIdentity::canonical_https_url),
        current: facts.current.clone(),
        status,
        verdict,
        ranked: ranked.into_iter().take(5).collect(),
    }
}

// ============================================================================
// Gathering facts
// ============================================================================

fn mapping_entries(mappings: &Value) -> Vec<&Value> {
    mappings
        .get("mappings")
        .and_then(Value::as_array)
        .or_else(|| mappings.as_array())
        .map(|entries| entries.iter().collect())
        .unwrap_or_default()
}

fn uuid_field(value: &Value, key: &str) -> Option<Uuid> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id.trim()).ok())
}

fn same_path(a: &str, b: &Path) -> bool {
    let a = Path::new(a.trim_end_matches('/'));
    a == b || std::fs::canonicalize(a).is_ok_and(|canonical| canonical == b)
}

/// The checkout's link as recorded locally: its checkout config, else the
/// machine's folder mapping for exactly this folder.
fn recorded_link(
    root: &Path,
    mappings: &Value,
) -> Option<(Uuid, Option<Uuid>, bool, &'static str)> {
    let config_path = root.join(".contextstream").join("config.json");
    if let Ok(raw) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = serde_json::from_str::<Value>(&raw) {
            if let Some(project_id) = uuid_field(&config, "project_id") {
                let fingerprinted = config.get("repository_fingerprint").is_some();
                return Some((
                    project_id,
                    uuid_field(&config, "workspace_id"),
                    fingerprinted,
                    "checkout_config",
                ));
            }
        }
    }
    mapping_entries(mappings)
        .into_iter()
        .find(|entry| {
            entry
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| same_path(path, root))
        })
        .and_then(|entry| {
            Some((
                uuid_field(entry, "project_id")?,
                uuid_field(entry, "workspace_id"),
                entry.get("repository_fingerprint").is_some(),
                "folder_mapping",
            ))
        })
}

/// Collect the local facts about the checkout containing `path`. `mappings`
/// is the parsed `~/.contextstream/mappings.json`.
pub fn gather_checkout_facts(path: &Path, mappings: &Value) -> CheckoutFacts {
    let layout = checkout_repository_layout(path);
    let root = layout
        .as_ref()
        .map(|layout| layout.repository_root.clone())
        .or_else(|| std::fs::canonicalize(path).ok())
        .unwrap_or_else(|| path.to_path_buf());
    let folder_name = root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let repository = current_repository_remote_identity(&root).ok().flatten();
    let repository_name = repository.as_ref().and_then(|identity| {
        identity
            .canonical_https_url()
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .map(str::to_string)
    });
    let main_checkout_name = layout
        .as_ref()
        .and_then(|layout| layout.main_checkout.as_ref())
        .and_then(|main| main.file_name())
        .map(|name| name.to_string_lossy().to_string());

    let fingerprint = current_repository_fingerprint(&root)
        .ok()
        .map(|fingerprint| fingerprint.to_string());
    let sibling_projects = fingerprint
        .map(|fingerprint| {
            mapping_entries(mappings)
                .into_iter()
                .filter(|entry| {
                    entry.get("repository_fingerprint").and_then(Value::as_str)
                        == Some(fingerprint.as_str())
                        && !entry
                            .get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|path| same_path(path, &root))
                })
                .filter_map(|entry| uuid_field(entry, "project_id"))
                .collect()
        })
        .unwrap_or_default();

    let current =
        recorded_link(&root, mappings).map(|(project_id, workspace_id, fingerprinted, source)| {
            CurrentLink {
                workspace_id,
                project_id,
                verified: fingerprinted,
                source,
            }
        });

    CheckoutFacts {
        root,
        folder_name,
        repository,
        repository_name,
        main_checkout_name,
        sibling_projects,
        current,
    }
}

/// Every project in every workspace the account can open. Workspaces that
/// refuse access are skipped.
pub async fn load_candidates(
    client: &mcp_client::ContextStreamClient,
) -> mcp_types::Result<Vec<ProjectCandidate>> {
    use futures::stream::{self, StreamExt};
    let workspaces = client.list_workspaces(Some(1), Some(100)).await?;
    let lists: Vec<Vec<ProjectCandidate>> = stream::iter(workspaces)
        .map(|workspace| async move {
            match client.list_all_projects(Some(workspace.id), 200, 5000).await {
                Ok(projects) => projects
                    .into_iter()
                    .map(|project| ProjectCandidate {
                        project,
                        workspace: workspace.clone(),
                    })
                    .collect(),
                Err(error) => {
                    tracing::debug!(%error, workspace = %workspace.id, "skipping workspace while resolving project links");
                    Vec::new()
                }
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;
    Ok(lists.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-06T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn workspace(name: &str, description: Option<&str>, team: bool) -> Workspace {
        Workspace {
            id: Uuid::new_v4(),
            name: name.to_string(),
            description: description.map(str::to_string),
            visibility: Some(if team { "team" } else { "private" }.to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn project(name: &str, repository: Option<&str>, path: Option<&str>, files: i64) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            description: None,
            repository_url: repository.map(str::to_string),
            repository_type: None,
            workspace_id: None,
            path: path.map(str::to_string),
            created_at: None,
            updated_at: Some("2026-10-01T00:00:00Z".to_string()),
            indexed_at: None,
            file_count: Some(files),
        }
    }

    fn candidate(project: Project, workspace: &Workspace) -> ProjectCandidate {
        ProjectCandidate {
            project,
            workspace: workspace.clone(),
        }
    }

    fn facts(folder: &str, repository: Option<&str>, main: Option<&str>) -> CheckoutFacts {
        let repository =
            repository.map(|url| RepositoryRemoteIdentity::from_remote_url(url).unwrap());
        let repository_name = repository.as_ref().map(|identity| {
            identity
                .canonical_https_url()
                .trim_end_matches(".git")
                .rsplit('/')
                .next()
                .unwrap()
                .to_string()
        });
        CheckoutFacts {
            root: PathBuf::from(format!("/Users/dev/src/{folder}")),
            folder_name: folder.to_string(),
            repository,
            repository_name,
            main_checkout_name: main.map(str::to_string),
            sibling_projects: Vec::new(),
            current: None,
        }
    }

    #[test]
    fn benchmark_link_loses_to_the_canonical_team_project() {
        let engineering = workspace("Engineering", Some("Engineering projects"), true);
        let benchmark = workspace(
            "Code Search Benchmark",
            Some("Isolated code-search benchmark projects: source index only"),
            false,
        );
        let canonical = project(
            "contextstream",
            Some("git@github.com:acme/contextstream.git"),
            Some("/home/dev/src/contextstream"),
            900,
        );
        let fixture = project(
            "contextstream",
            Some("https://github.com/acme/contextstream"),
            Some("/Users/dev/src/contextstream"),
            900,
        );
        let worktree_project = project(
            "contextstream-index-reseed-20260729",
            None,
            Some("/home/dev/src/contextstream-index-reseed-20260729"),
            10,
        );
        let candidates = vec![
            candidate(canonical.clone(), &engineering),
            candidate(fixture.clone(), &benchmark),
            candidate(worktree_project, &engineering),
        ];
        let mut checkout = facts(
            "contextstream",
            Some("git@github.com:acme/contextstream.git"),
            None,
        );
        checkout.current = Some(CurrentLink {
            workspace_id: Some(benchmark.id),
            project_id: fixture.id,
            verified: true,
            source: "checkout_config",
        });

        let assessment = assess(&checkout, &candidates, now());
        let Verdict::Confident { best } = &assessment.verdict else {
            panic!("expected a confident verdict: {assessment:#?}");
        };
        assert_eq!(best.project_id, canonical.id);
        assert!(matches!(assessment.status, LinkStatus::Wrong { .. }));
        assert!(assessment.safe_to_auto_fix());
    }

    #[test]
    fn dated_worktree_resolves_to_the_main_project_not_its_own() {
        let engineering = workspace("Engineering", None, true);
        let canonical = project(
            "contextstream",
            None,
            Some("/home/dev/src/contextstream"),
            900,
        );
        let own = project(
            "contextstream-validation-opt-in-20261002",
            None,
            Some("/Users/dev/src/contextstream-validation-opt-in-20261002"),
            50,
        );
        let candidates = vec![
            candidate(canonical.clone(), &engineering),
            candidate(own.clone(), &engineering),
        ];
        let mut checkout = facts(
            "contextstream-validation-opt-in-20261002",
            Some("https://github.com/acme/contextstream.git"),
            Some("contextstream"),
        );
        checkout.current = Some(CurrentLink {
            workspace_id: Some(engineering.id),
            project_id: own.id,
            verified: false,
            source: "folder_mapping",
        });
        let assessment = assess(&checkout, &candidates, now());
        let Verdict::Confident { best } = &assessment.verdict else {
            panic!("expected confident: {assessment:#?}");
        };
        assert_eq!(best.project_id, canonical.id);
        assert!(matches!(assessment.status, LinkStatus::Wrong { .. }));
        // Names alone are not strong enough for an unattended repair.
        assert!(!assessment.safe_to_auto_fix());
    }

    #[test]
    fn inaccessible_link_is_broken_and_sibling_checkout_is_strong_evidence() {
        let engineering = workspace("Engineering", None, true);
        let canonical = project("mcp-server", None, None, 300);
        let other = project("mcp", None, None, 300);
        let candidates = vec![
            candidate(canonical.clone(), &engineering),
            candidate(other, &engineering),
        ];
        let mut checkout = facts("mcp-server", None, None);
        checkout.sibling_projects = vec![canonical.id];
        checkout.current = Some(CurrentLink {
            workspace_id: Some(Uuid::new_v4()),
            project_id: Uuid::new_v4(),
            verified: false,
            source: "folder_mapping",
        });
        let assessment = assess(&checkout, &candidates, now());
        assert!(matches!(assessment.status, LinkStatus::Broken { .. }));
        assert_eq!(
            assessment.recommended().map(|p| p.project_id),
            Some(canonical.id)
        );
        assert!(assessment.safe_to_auto_fix());
    }

    #[test]
    fn close_calls_are_ambiguous_and_never_auto_fixed() {
        let a = workspace("Engineering", None, true);
        let b = workspace("Marketing", None, true);
        // Two projects in different workspaces claim the same repository.
        let one = project("site", Some("https://github.com/acme/site.git"), None, 10);
        let two = project("site", Some("git@github.com:acme/site.git"), None, 10);
        let candidates = vec![candidate(one, &a), candidate(two, &b)];
        let checkout = facts("site", Some("https://github.com/acme/site"), None);
        let assessment = assess(&checkout, &candidates, now());
        assert!(
            matches!(assessment.verdict, Verdict::Ambiguous { .. }),
            "{assessment:#?}"
        );
        assert!(!assessment.safe_to_auto_fix());
    }

    #[test]
    fn existing_link_breaks_a_tie_between_duplicate_projects() {
        let engineering = workspace("Engineering", None, true);
        let sales = workspace("Sales", None, true);
        let ours = project(
            "contextcode",
            Some("https://github.com/acme/contextcode.git"),
            None,
            500,
        );
        let duplicate = project(
            "contextcode",
            Some("https://github.com/acme/contextcode.git"),
            None,
            0,
        );
        let candidates = vec![
            candidate(ours.clone(), &engineering),
            candidate(duplicate, &sales),
        ];
        let mut checkout = facts(
            "contextcode",
            Some("git@github.com:acme/contextcode.git"),
            None,
        );
        assert!(matches!(
            assess(&checkout, &candidates, now()).verdict,
            Verdict::Ambiguous { .. }
        ));
        checkout.current = Some(CurrentLink {
            workspace_id: Some(engineering.id),
            project_id: ours.id,
            verified: false,
            source: "folder_mapping",
        });
        let assessment = assess(&checkout, &candidates, now());
        assert_eq!(
            assessment.recommended().map(|p| p.project_id),
            Some(ours.id)
        );
        assert_eq!(assessment.status, LinkStatus::Unverified);
    }

    #[test]
    fn correct_verified_link_needs_nothing() {
        let engineering = workspace("Engineering", None, true);
        let canonical = project("app", Some("https://github.com/acme/app.git"), None, 30);
        let candidates = vec![candidate(canonical.clone(), &engineering)];
        let mut checkout = facts("app", Some("git@github.com:acme/app.git"), None);
        checkout.current = Some(CurrentLink {
            workspace_id: Some(engineering.id),
            project_id: canonical.id,
            verified: true,
            source: "checkout_config",
        });
        let assessment = assess(&checkout, &candidates, now());
        assert_eq!(assessment.status, LinkStatus::Correct);
        assert!(assessment.recommended().is_none());
        assert!(!assessment.safe_to_auto_fix());

        checkout.current.as_mut().unwrap().verified = false;
        let unverified = assess(&checkout, &candidates, now());
        assert_eq!(unverified.status, LinkStatus::Unverified);
        assert!(
            unverified.safe_to_auto_fix(),
            "re-establishing the same link is safe"
        );
    }

    #[test]
    fn generated_names_and_isolated_workspaces_are_recognized() {
        for name in [
            "project-dcc7239f",
            "contextstream-validation-opt-in-20261002",
            "cslive_pr_hygiene_retest470_20261005T005307Z.qIqC",
            "cs-release-07229",
        ] {
            assert!(is_generated_name(name), "{name}");
        }
        for name in [
            "contextstream",
            "mcp-server",
            "coflow-orbit-archive",
            "sp",
            "agents",
        ] {
            assert!(!is_generated_name(name), "{name}");
        }
        assert!(is_generated_path("/private/tmp/cslive_x/repo"));
        assert!(is_isolated_workspace(&workspace(
            "LongMemEval",
            Some("Isolation workspace for the benchmark harness"),
            false
        )));
        assert!(!is_isolated_workspace(&workspace(
            "Engineering",
            Some("Engineering projects for ContextStream"),
            true
        )));
    }

    #[test]
    fn recorded_links_come_from_checkout_config_then_exact_mapping() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project_id = Uuid::new_v4();
        let mappings = serde_json::json!({"mappings": [
            {"path": root.to_string_lossy(), "project_id": project_id.to_string()},
            {"path": root.join("sub").to_string_lossy(), "project_id": Uuid::new_v4().to_string()}
        ]});
        let link = recorded_link(&root, &mappings).unwrap();
        assert_eq!(link.0, project_id);
        assert_eq!(link.3, "folder_mapping");
        assert!(!link.2);

        let config_project = Uuid::new_v4();
        std::fs::create_dir_all(root.join(".contextstream")).unwrap();
        std::fs::write(
            root.join(".contextstream/config.json"),
            serde_json::json!({"project_id": config_project.to_string(), "repository_fingerprint": "x"}).to_string(),
        )
        .unwrap();
        let link = recorded_link(&root, &mappings).unwrap();
        assert_eq!(link.0, config_project);
        assert_eq!(link.3, "checkout_config");
        assert!(link.2);
    }
}
