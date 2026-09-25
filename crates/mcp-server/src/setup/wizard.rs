//! Interactive `contextstream-mcp setup`.
//!
//! Sign in, then everything setup can decide safely is decided for the user:
//! detected editors, the folder's existing link or Git-repository match, the
//! only workspace, a new project named after the folder, and background
//! indexing. One review screen is the single confirmation and the place to
//! change any of those choices. Nothing on the server or on disk changes
//! before the user chooses Save, except the sign-in itself.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcp_client::ContextStreamClient;
use mcp_types::Config;

use super::ui::{self, ui, Mark};
use super::{
    editors, gather_project_candidates, git_hooks, prompts, safe_edit, ProjectInfo,
    SetupIndexChoice, SetupTransportPreference, WorkspaceInfo,
};

const STEPS: [&str; 5] = ["Account", "Editors", "Project", "Review", "Finish"];

/// Print the stepper and a titled section for wizard step `step` (1-based).
pub(super) fn print_step(step: usize, title: &str, detail: &str) {
    let ui = ui();
    println!();
    println!("{}{}", ui::GUTTER, ui.stepper(&STEPS, step));
    println!();
    println!("{}{}", ui::GUTTER, ui.heading(title));
    if !detail.is_empty() {
        println!("{}{}", ui::GUTTER, ui.muted(detail));
    }
    println!();
}

/// Shorten paths under HOME to `~/…` for display.
pub(super) fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(relative) = path.strip_prefix(&home) {
            return if relative.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", relative.display())
            };
        }
    }
    path.display().to_string()
}

/// What setup will link this folder to once the user saves.
#[derive(Clone, Debug)]
pub(super) enum ProjectPlan {
    /// No project: account and editor setup only.
    Unlinked,
    Existing {
        project: ProjectInfo,
        reason: &'static str,
    },
    /// Created on Save, so backing out of the review leaves nothing behind.
    Create {
        name: String,
        workspace_id: Option<uuid::Uuid>,
        repository_url: Option<String>,
    },
}

impl ProjectPlan {
    fn summary(&self) -> String {
        match self {
            ProjectPlan::Unlinked => "Not linked · editors only".to_string(),
            ProjectPlan::Existing { project, reason } => format!("{} · {reason}", project.name),
            ProjectPlan::Create { name, .. } => format!("{name} · new project"),
        }
    }

    fn is_linked(&self) -> bool {
        !matches!(self, ProjectPlan::Unlinked)
    }
}

/// When the first index runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexPlan {
    Background,
    Now,
    Skip,
    /// An empty folder: files are synced as they appear.
    NothingYet,
    /// No project is linked.
    NotApplicable,
}

impl IndexPlan {
    fn summary(self) -> &'static str {
        match self {
            IndexPlan::Background => "In the background after setup",
            IndexPlan::Now => "Now, before setup finishes",
            IndexPlan::Skip => "Skipped for now",
            IndexPlan::NothingYet => "Nothing to index yet · files sync as you add them",
            IndexPlan::NotApplicable => "No project linked",
        }
    }

    fn from_choice(choice: SetupIndexChoice) -> Self {
        match choice {
            SetupIndexChoice::Background => IndexPlan::Background,
            SetupIndexChoice::Foreground => IndexPlan::Now,
            SetupIndexChoice::Skip => IndexPlan::Skip,
        }
    }
}

pub(super) fn default_index_plan(project_path: Option<&Path>, plan: &ProjectPlan) -> IndexPlan {
    match project_path {
        Some(path) if plan.is_linked() => {
            if super::setup_path_has_project_content_files(path, false) {
                IndexPlan::Background
            } else {
                IndexPlan::NothingYet
            }
        }
        _ => IndexPlan::NotApplicable,
    }
}

/// Everything the review screen shows and Save applies.
struct Choices {
    api_key: String,
    email: String,
    client: ContextStreamClient,
    team_capable: bool,
    editors: Vec<editors::Editor>,
    transport: SetupTransportPreference,
    project_path: Option<PathBuf>,
    account_only: bool,
    workspace: Option<WorkspaceInfo>,
    project: ProjectPlan,
    index: IndexPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewAction {
    Save,
    Editors,
    Project,
    Indexing,
    Account,
    Exit,
}

fn review_actions(index: IndexPlan) -> Vec<(ReviewAction, &'static str)> {
    let mut actions = vec![
        (ReviewAction::Save, "Save and finish"),
        (ReviewAction::Editors, "Change editors"),
        (ReviewAction::Project, "Change workspace or project"),
    ];
    if matches!(
        index,
        IndexPlan::Background | IndexPlan::Now | IndexPlan::Skip
    ) {
        actions.push((ReviewAction::Indexing, "Change when indexing runs"));
    }
    actions.push((ReviewAction::Account, "Sign in with a different account"));
    actions.push((ReviewAction::Exit, "Exit without saving"));
    actions
}

/// Interactive setup entry point (see the module docs).
pub(super) async fn run(
    only: Option<&[editors::Editor]>,
    explicit_project_path: Option<&Path>,
    account_only: bool,
) -> Result<()> {
    super::print_welcome_banner();
    super::print_data_collection_disclosure(false);

    // Whether inline email + SMS signup is offered depends on the server;
    // ask now so the sign-in menu never waits on it.
    let signup_probe = tokio::spawn(super::inline_signup_available());

    print_step(
        1,
        "Sign in to ContextStream",
        "Your agents' memory lives in your ContextStream account.",
    );
    let (api_key, email) = sign_in(Some(signup_probe)).await?;
    let client = client_for(&api_key);
    let team_capable = load_team_capability(&client).await;

    print_step(
        2,
        "Connect your editors",
        "Each one gets the ContextStream MCP server, rules, and hooks.",
    );
    let detected = editors::detect_installed_editors();
    let editors = initial_editors(only, &detected)?;
    let transport = super::prompt_setup_transport_preference(&editors)?;

    print_step(
        3,
        "Link this project",
        "So your agents can search the code and recall decisions made here.",
    );
    if team_capable {
        super::team_guidance::print_workspace_step_team_tips();
    }
    let cwd = std::env::current_dir()?;
    let (project_path, account_only) =
        match super::resolve_setup_project_path(&cwd, explicit_project_path, account_only)? {
            Some(path) => (Some(path), false),
            None if account_only => (None, true),
            None => prompt_project_path(&cwd)?,
        };
    let workspace = choose_workspace(&client, project_path.as_deref(), true).await?;
    let project = plan_project(&client, project_path.as_deref(), workspace.as_ref()).await?;
    let index = default_index_plan(project_path.as_deref(), &project);

    let mut choices = Choices {
        api_key,
        email,
        client,
        team_capable,
        editors,
        transport,
        project_path,
        account_only,
        workspace,
        project,
        index,
    };

    loop {
        print_step(
            4,
            "Review",
            "Nothing is saved until you choose Save. Change anything first.",
        );
        print_review(&choices);
        let actions = review_actions(choices.index);
        let labels: Vec<&str> = actions.iter().map(|(_, label)| *label).collect();
        let choice = prompts::select("Ready to set up ContextStream?", &labels)?;
        match actions[choice].0 {
            ReviewAction::Save => break,
            ReviewAction::Editors => {
                choices.editors =
                    prompts::select_editors_with_defaults(&detected, &choices.editors)?;
                choices.transport = super::prompt_setup_transport_preference(&choices.editors)?;
            }
            ReviewAction::Project => {
                choices.workspace =
                    choose_workspace(&choices.client, choices.project_path.as_deref(), false)
                        .await?;
                choices.project = pick_project(
                    &choices.client,
                    choices.project_path.as_deref(),
                    choices.workspace.as_ref(),
                )
                .await?;
                choices.index =
                    default_index_plan(choices.project_path.as_deref(), &choices.project);
            }
            ReviewAction::Indexing => {
                if let Some(path) = choices.project_path.as_deref() {
                    choices.index = IndexPlan::from_choice(super::prompt_setup_index_choice(path)?);
                }
            }
            ReviewAction::Account => {
                let (api_key, email) = sign_in(None).await?;
                choices.client = client_for(&api_key);
                choices.api_key = api_key;
                choices.email = email;
                choices.team_capable = load_team_capability(&choices.client).await;
                choices.workspace =
                    choose_workspace(&choices.client, choices.project_path.as_deref(), true)
                        .await?;
                choices.project = plan_project(
                    &choices.client,
                    choices.project_path.as_deref(),
                    choices.workspace.as_ref(),
                )
                .await?;
                choices.index =
                    default_index_plan(choices.project_path.as_deref(), &choices.project);
            }
            ReviewAction::Exit => {
                ui::say(Mark::Pending, "Setup cancelled", Some("nothing was saved"));
                println!();
                return Ok(());
            }
        }
    }

    print_step(5, "Finishing setup", "");
    finish(choices).await
}

fn client_for(api_key: &str) -> ContextStreamClient {
    ContextStreamClient::new(Config {
        api_key: Some(api_key.to_string()),
        ..Default::default()
    })
}

async fn load_team_capability(client: &ContextStreamClient) -> bool {
    let context = ui::spin(
        "Loading your account",
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_account_context(),
        ),
    )
    .await;
    context
        .ok()
        .and_then(|result| result.ok())
        .flatten()
        .filter(|ctx| ctx.team_features_available())
        .inspect(super::team_guidance::print_post_auth_team_guidance)
        .is_some()
}

/// Reuse a working saved sign-in without asking; otherwise sign in fresh.
async fn sign_in(signup_probe: Option<tokio::task::JoinHandle<bool>>) -> Result<(String, String)> {
    if safe_edit::is_dry_run() {
        let api_key = super::get_api_key_result()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Dry-run requires existing credentials because browser authentication would \
                 create server-side state. Authenticate once, then re-run --dry-run."
            )
        })?;
        let user = ui::spin("Checking your sign-in", client_for(&api_key).me())
            .await
            .map_err(|error| {
                anyhow::anyhow!("Dry-run could not validate the existing credentials: {error}")
            })?;
        ui::say(Mark::Ok, "Signed in", Some(&user.email));
        return Ok((api_key, user.email));
    }

    // Only the first sign-in reuses saved credentials; switching accounts
    // from the review screen always signs in fresh.
    if signup_probe.is_some() {
        if let Some(api_key) = super::read_saved_credentials()
            .ok()
            .and_then(|creds| creds.api_key)
            .filter(|key| !key.trim().is_empty())
        {
            let check = ui::spin(
                "Checking your saved sign-in",
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client_for(&api_key).me(),
                ),
            )
            .await;
            match check {
                Ok(Ok(user)) => {
                    ui::say(Mark::Ok, "Signed in", Some(&user.email));
                    return Ok((api_key, user.email));
                }
                Ok(Err(error)) => ui::say(
                    Mark::Warn,
                    "Your saved sign-in no longer works",
                    Some(&error.to_string()),
                ),
                Err(_) => ui::say(
                    Mark::Warn,
                    "Couldn't check your saved sign-in",
                    Some("the server didn't answer in time"),
                ),
            }
        }
    }

    let signup_available = match signup_probe {
        Some(probe) => tokio::time::timeout(std::time::Duration::from_millis(1500), probe)
            .await
            .ok()
            .and_then(|joined| joined.ok())
            .unwrap_or(false),
        None => super::inline_signup_available().await,
    };
    let (api_key, email) = super::authenticate_fresh(signup_available).await?;
    super::write_saved_credentials(&api_key, None)?;
    ui::say(
        Mark::Ok,
        "Signed in",
        Some(&format!(
            "{email} · saved to {}",
            display_path(&super::credentials_file_path())
        )),
    );
    Ok((api_key, email))
}

fn initial_editors(
    only: Option<&[editors::Editor]>,
    detected: &[editors::Editor],
) -> Result<Vec<editors::Editor>> {
    if let Some(requested) = only {
        ui::say(
            Mark::Ok,
            "Editors",
            Some(&format!(
                "{} · from --editors",
                super::selected_editors_summary(requested)
            )),
        );
        return Ok(requested.to_vec());
    }
    if detected.is_empty() {
        ui::say(
            Mark::Info,
            "No editors found on this machine",
            Some("pick the ones you use"),
        );
        return prompts::select_editors(detected);
    }
    ui::say(
        Mark::Ok,
        "Editors",
        Some(&format!(
            "{} · detected",
            super::selected_editors_summary(detected)
        )),
    );
    Ok(detected.to_vec())
}

/// The current folder is HOME or a filesystem root: default to editor-only
/// setup rather than asking for a path up front.
fn prompt_project_path(cwd: &Path) -> Result<(Option<PathBuf>, bool)> {
    ui::say(
        Mark::Info,
        &format!("{} is too broad to link as a project", display_path(cwd)),
        Some("open a project folder, or link one now"),
    );
    let choice = prompts::select(
        "Which project should ContextStream learn?",
        &[
            "None yet · set up editors now, link a project later",
            "Enter a project folder",
        ],
    )?;
    if choice == 0 {
        return Ok((None, true));
    }
    loop {
        let raw = prompts::input("Project folder", None)?;
        match super::canonical_setup_project_path(Path::new(raw.trim()), cwd) {
            Ok(path) => return Ok((Some(path), false)),
            Err(error) => {
                ui::say(Mark::Warn, &error.to_string(), None);
                let retry = prompts::select(
                    "Try again?",
                    &["Enter another folder", "Set up editors only for now"],
                )?;
                if retry == 1 {
                    return Ok((None, true));
                }
            }
        }
    }
}

/// Resolve the workspace. With `auto`, the folder's previous workspace, the
/// only workspace, or a newly created first workspace is chosen without a
/// prompt; otherwise the user picks.
async fn choose_workspace(
    client: &ContextStreamClient,
    project_path: Option<&Path>,
    auto: bool,
) -> Result<Option<WorkspaceInfo>> {
    let previous = project_path.and_then(|path| super::read_project_config(path).ok().flatten());
    let workspaces = ui::spin("Loading workspaces", client.list_workspaces(None, None)).await?;

    if auto {
        let linked = previous.as_ref().and_then(|config| {
            config.workspace_id.as_ref().and_then(|id| {
                workspaces
                    .iter()
                    .find(|workspace| workspace.id.to_string() == *id)
            })
        });
        let (chosen, reason) = match (linked, workspaces.as_slice()) {
            (Some(workspace), _) => (Some(workspace), "used by this folder before"),
            (None, [only]) => (Some(only), ""),
            _ => (None, ""),
        };
        if let Some(workspace) = chosen {
            let detail = if reason.is_empty() {
                workspace.name.clone()
            } else {
                format!("{} · {reason}", workspace.name)
            };
            ui::say(Mark::Ok, "Workspace", Some(&detail));
            return Ok(Some(WorkspaceInfo {
                id: workspace.id.to_string(),
                name: workspace.name.clone(),
            }));
        }
        if workspaces.is_empty() {
            return create_workspace(client, "My Workspace").await.map(Some);
        }
    }

    let mut labels: Vec<String> = workspaces.iter().map(|w| w.name.clone()).collect();
    labels.push("Create a new workspace".to_string());
    labels.push("None · set up editors only".to_string());
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let choice = prompts::select("Which workspace?", &label_refs)?;
    if let Some(workspace) = workspaces.get(choice) {
        return Ok(Some(WorkspaceInfo {
            id: workspace.id.to_string(),
            name: workspace.name.clone(),
        }));
    }
    if choice == workspaces.len() {
        let name = prompts::input("Workspace name", Some("My Workspace"))?;
        return create_workspace(client, name.trim()).await.map(Some);
    }
    Ok(None)
}

async fn create_workspace(client: &ContextStreamClient, name: &str) -> Result<WorkspaceInfo> {
    if safe_edit::is_dry_run() {
        anyhow::bail!(
            "Dry-run refused to create a server-side workspace. \
             Create it first, then re-run the preview."
        );
    }
    let workspace = ui::spin(
        "Creating your workspace",
        client.create_workspace(name, None),
    )
    .await?;
    ui::say(
        Mark::Ok,
        "Workspace",
        Some(&format!("{} · created", workspace.name)),
    );
    Ok(WorkspaceInfo {
        id: workspace.id.to_string(),
        name: workspace.name,
    })
}

/// Decide the project without prompting when the answer is unambiguous: the
/// folder's validated link, then the one project bound to this Git
/// repository, then a new project named after the folder. A same-name
/// project or several repository matches are never assumed; the user picks.
async fn plan_project(
    client: &ContextStreamClient,
    project_path: Option<&Path>,
    workspace: Option<&WorkspaceInfo>,
) -> Result<ProjectPlan> {
    let (Some(path), Some(workspace)) = (project_path, workspace) else {
        return Ok(ProjectPlan::Unlinked);
    };
    let candidates = ui::spin(
        "Looking for this project",
        gather_project_candidates(client, path, workspace),
    )
    .await?;

    let plan = if let Some(project) = candidates.linked_project.as_ref() {
        Some(ProjectPlan::Existing {
            project: ProjectInfo::from(project),
            reason: "linked to this folder",
        })
    } else if let [project] = candidates.repository_matches.as_slice() {
        Some(ProjectPlan::Existing {
            project: ProjectInfo::from(project),
            reason: "matches this Git repository",
        })
    } else if candidates.repository_matches.is_empty() && candidates.folder_match.is_none() {
        Some(ProjectPlan::Create {
            name: candidates.folder_project_name.clone(),
            workspace_id: candidates.ws_id,
            repository_url: candidates.checkout_repository_url.clone(),
        })
    } else {
        None
    };

    match plan {
        Some(plan) => {
            ui::say(Mark::Ok, "Project", Some(&plan.summary()));
            Ok(plan)
        }
        None => pick_project(client, project_path, Some(workspace)).await,
    }
}

/// Let the user choose; picking "create" creates the project immediately
/// because it is an explicit choice.
async fn pick_project(
    client: &ContextStreamClient,
    project_path: Option<&Path>,
    workspace: Option<&WorkspaceInfo>,
) -> Result<ProjectPlan> {
    let Some(path) = project_path else {
        return Ok(ProjectPlan::Unlinked);
    };
    let selected =
        super::select_project_for_current_directory(client, path, workspace, true, true, true)
            .await?;
    Ok(match selected {
        Some(project) => ProjectPlan::Existing {
            project,
            reason: "chosen",
        },
        None => ProjectPlan::Unlinked,
    })
}

fn print_review(choices: &Choices) {
    let ui = ui();
    let mut account = choices.email.clone();
    if choices.team_capable {
        account.push_str(&ui.faint(" · team"));
    }
    let mut rows = vec![
        ui.row("Account", &account),
        ui.row(
            "Editors",
            &if choices.editors.is_empty() {
                ui.muted("None")
            } else {
                super::selected_editors_summary(&choices.editors)
            },
        ),
        ui.row(
            "Workspace",
            &choices
                .workspace
                .as_ref()
                .map(|workspace| workspace.name.clone())
                .unwrap_or_else(|| ui.muted("None")),
        ),
        ui.row("Project", &choices.project.summary()),
    ];
    if let Some(path) = choices.project_path.as_deref() {
        rows.push(ui.row("Folder", &ui.path(&display_path(path))));
    }
    rows.push(ui.row("Indexing", choices.index.summary()));
    if matches!(choices.transport, SetupTransportPreference::LocalBinary) {
        rows.push(ui.row("Connection", &ui.warning("Local binary · recovery mode")));
    }
    println!("{}", ui.card(&rows));
    println!();
}

async fn finish(choices: Choices) -> Result<()> {
    let Choices {
        api_key,
        email,
        client,
        team_capable,
        editors,
        transport,
        project_path,
        mut account_only,
        workspace,
        project,
        index,
    } = choices;

    super::persist_setup_editor_selection(&editors)?;

    let selected_project = materialize_project(&client, project).await?;
    if selected_project.is_none() {
        account_only = true;
    }
    let configured_project_path = selected_project.as_ref().and(project_path.as_deref());

    let preauth = matches!(transport, SetupTransportPreference::HostedRemote)
        && editors
            .iter()
            .any(super::mcp_config::editor_supports_remote_mcp);
    let mut failed_editors = Vec::new();
    if !editors.is_empty() {
        // Persist transport intent before the first editor mutation so a
        // partial setup remains repairable with the same connection mode.
        super::write_setup_transport_marker(transport)?;
        for editor in &editors {
            // One editor failing (a locked or malformed config file) must not
            // stop the others; the failure is listed and repair is offered.
            if let Err(error) = super::configure_editor_with_workspace(
                &client,
                editor,
                &api_key,
                workspace.as_ref(),
                selected_project.as_ref().map(|p| p.id.as_str()),
                configured_project_path,
                transport,
                preauth,
            )
            .await
            {
                tracing::warn!("{error:#}");
                failed_editors.push(editor.display_name());
            }
        }
    }

    let binding_established = match (configured_project_path, workspace.as_ref()) {
        (Some(path), Some(workspace)) => {
            super::establish_validated_setup_binding(
                &client,
                path,
                workspace,
                selected_project.as_ref(),
            )
            .await?
        }
        _ => false,
    };
    if binding_established {
        if let Some(path) = configured_project_path {
            ui::say(Mark::Ok, "Project linked", Some(&display_path(path)));
            install_git_capture(path);
        }
    }

    if !safe_edit::is_dry_run() {
        let telemetry_client = client.clone();
        let _telemetry = tokio::spawn(async move {
            super::report_setup_telemetry(&telemetry_client, transport).await;
        });
    }

    let workspace_id = workspace.as_ref().map(|w| &w.id);
    let (index_started, awaiting_first_files) =
        match configured_project_path.filter(|_| binding_established) {
            None => {
                if !account_only {
                    ui::say(
                        Mark::Pending,
                        "Nothing was indexed",
                        Some("no project is linked"),
                    );
                }
                super::spawn_warmup(&client, workspace_id);
                (false, false)
            }
            Some(_) if index == IndexPlan::NothingYet => {
                super::print_empty_project_ready();
                super::spawn_warmup(&client, workspace_id);
                (false, true)
            }
            Some(path) => {
                run_index(
                    &client,
                    path,
                    workspace.as_ref(),
                    selected_project.as_ref(),
                    index,
                )
                .await
            }
        };

    if !failed_editors.is_empty() {
        ui::say(
            Mark::Warn,
            &format!("{} couldn't be configured", failed_editors.join(", ")),
            Some("details above; the repair command is below"),
        );
    }

    let report = ui::spin(
        "Checking everything works",
        super::doctor::build_report(configured_project_path, &editors),
    )
    .await;
    super::doctor::print_setup_health_report(&report);
    let outcome = super::setup_completion_evidence(
        editors.len(),
        editors
            .iter()
            .filter(|editor| editor.has_mcp_transport())
            .count(),
        project_path.is_some(),
        selected_project.is_some(),
        binding_established,
        index_started,
        awaiting_first_files,
        !report.has_setup_failures() && failed_editors.is_empty(),
        account_only,
        safe_edit::is_dry_run(),
    );
    super::print_setup_outcome(
        &email,
        team_capable,
        &editors,
        workspace.as_ref(),
        project_path.as_deref(),
        &outcome,
    );
    Ok(())
}

async fn materialize_project(
    client: &ContextStreamClient,
    plan: ProjectPlan,
) -> Result<Option<ProjectInfo>> {
    match plan {
        ProjectPlan::Unlinked => Ok(None),
        ProjectPlan::Existing { project, .. } => Ok(Some(project)),
        ProjectPlan::Create {
            name,
            workspace_id,
            repository_url,
        } => {
            if safe_edit::is_dry_run() {
                ui::say(
                    Mark::Info,
                    &format!("Would create project {name}"),
                    Some("dry run · nothing was created"),
                );
                return Ok(None);
            }
            let project = ui::spin(
                &format!("Creating project {name}"),
                client.create_project_with_repository(
                    &name,
                    None,
                    workspace_id,
                    repository_url.as_deref(),
                ),
            )
            .await?;
            ui::say(Mark::Ok, "Project created", Some(&project.name));
            Ok(Some(ProjectInfo {
                id: project.id.to_string(),
                name: project.name,
            }))
        }
    }
}

/// Managed git hooks for local VCS capture (best effort; honors the capture
/// kill switch and per-repo policy; a no-op outside a git repository).
fn install_git_capture(path: &Path) {
    let Some(repo_root) = git_hooks::resolve_repo_root(path) else {
        return;
    };
    let root = repo_root.to_string_lossy().to_string();
    if crate::hook_handlers::git_common::capture_disabled(&root) {
        ui::say(Mark::Info, "Git capture is off for this repository", None);
        return;
    }
    match git_hooks::install_git_hooks(&repo_root) {
        Ok(()) => ui::say(
            Mark::Ok,
            "Git capture on",
            Some("commits, pushes, checkouts, and merges"),
        ),
        Err(error) => ui::say(
            Mark::Warn,
            "Couldn't install git hooks",
            Some(&error.to_string()),
        ),
    }
}

/// Start the first index as planned. Returns (index started, awaiting files).
async fn run_index(
    client: &ContextStreamClient,
    path: &Path,
    workspace: Option<&WorkspaceInfo>,
    project: Option<&ProjectInfo>,
    index: IndexPlan,
) -> (bool, bool) {
    let workspace_id = workspace.map(|w| &w.id);
    let project_id = project.and_then(|project| uuid::Uuid::parse_str(&project.id).ok());
    match index {
        IndexPlan::Background => {
            super::spawn_background_index(
                client.clone(),
                path.to_path_buf(),
                workspace.map(|w| w.id.clone()),
                project_id,
                false,
            );
            (true, false)
        }
        IndexPlan::Now => {
            match super::index_project(client, path, workspace_id, project_id, false, false).await {
                Ok(()) => {
                    super::spawn_warmup(client, workspace_id);
                    (true, false)
                }
                Err(error) => {
                    ui::say(
                        Mark::Warn,
                        "Indexing didn't finish",
                        Some(&super::sanitize_index_error(&error)),
                    );
                    (false, false)
                }
            }
        }
        IndexPlan::Skip => {
            ui::say(
                Mark::Pending,
                "Indexing skipped",
                Some("ask your agent to run project(action=\"index\") when ready"),
            );
            super::spawn_warmup(client, workspace_id);
            (false, false)
        }
        IndexPlan::NothingYet | IndexPlan::NotApplicable => {
            super::spawn_warmup(client, workspace_id);
            (false, index == IndexPlan::NothingYet)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing(name: &str) -> ProjectPlan {
        ProjectPlan::Existing {
            project: ProjectInfo {
                id: uuid::Uuid::nil().to_string(),
                name: name.to_string(),
            },
            reason: "linked to this folder",
        }
    }

    #[test]
    fn project_summaries_never_show_ids() {
        assert_eq!(existing("api").summary(), "api · linked to this folder");
        let create = ProjectPlan::Create {
            name: "web".into(),
            workspace_id: Some(uuid::Uuid::nil()),
            repository_url: None,
        };
        assert_eq!(create.summary(), "web · new project");
        assert!(!create.summary().contains("0000"));
        assert_eq!(ProjectPlan::Unlinked.summary(), "Not linked · editors only");
    }

    #[test]
    fn indexing_defaults_to_background_only_for_linked_folders_with_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            default_index_plan(Some(dir.path()), &existing("empty")),
            IndexPlan::NothingYet
        );
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").expect("write");
        assert_eq!(
            default_index_plan(Some(dir.path()), &existing("app")),
            IndexPlan::Background
        );
        assert_eq!(
            default_index_plan(Some(dir.path()), &ProjectPlan::Unlinked),
            IndexPlan::NotApplicable
        );
        assert_eq!(
            default_index_plan(None, &existing("app")),
            IndexPlan::NotApplicable
        );
    }

    #[test]
    fn review_offers_indexing_changes_only_when_there_is_something_to_index() {
        let with_index: Vec<_> = review_actions(IndexPlan::Background)
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        assert_eq!(with_index.first(), Some(&ReviewAction::Save));
        assert!(with_index.contains(&ReviewAction::Indexing));
        assert_eq!(with_index.last(), Some(&ReviewAction::Exit));

        let empty: Vec<_> = review_actions(IndexPlan::NothingYet)
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        assert!(!empty.contains(&ReviewAction::Indexing));
    }

    /// Prints every setup screen in truecolor for visual review:
    /// `cargo test -p mcp-server --lib render_gallery -- --ignored --nocapture`.
    #[test]
    #[ignore = "visual review aid; prints ANSI screens"]
    fn render_gallery() {
        use dialoguer::theme::Theme;

        let width = 100;
        let fill = std::env::var("GALLERY_FILL").is_ok_and(|value| value == "1");
        let variant = if std::env::var("GALLERY_LIGHT").is_ok() {
            ui::Variant::Light
        } else {
            ui::Variant::Dark
        };
        assert!(ui::install_for_test(ui::Ui::fixed(
            ui::ColorDepth::TrueColor,
            variant,
            fill,
            width
        )));
        let ui = ui();

        super::super::print_welcome_banner();
        super::super::print_data_collection_disclosure(false);
        print_step(
            1,
            "Sign in to ContextStream",
            "Your agents' memory lives in your ContextStream account.",
        );
        ui::say(Mark::Ok, "Signed in", Some("you@example.com"));
        print_step(
            2,
            "Connect your editors",
            "Each one gets the ContextStream MCP server, rules, and hooks.",
        );
        ui::say(Mark::Ok, "Editors", Some("Claude Code, Cursor · detected"));
        print_step(
            3,
            "Link this project",
            "So your agents can search the code and recall decisions made here.",
        );
        ui::say(Mark::Ok, "Workspace", Some("Personal"));
        ui::say(
            Mark::Ok,
            "Project",
            Some("acme-web · matches this Git repository"),
        );
        print_step(
            4,
            "Review",
            "Nothing is saved until you choose Save. Change anything first.",
        );

        let choices = Choices {
            api_key: String::new(),
            email: "you@example.com".into(),
            client: client_for("test"),
            team_capable: true,
            editors: vec![editors::Editor::ClaudeCode, editors::Editor::Cursor],
            transport: SetupTransportPreference::HostedRemote,
            project_path: dirs::home_dir().map(|home| home.join("code").join("acme-web")),
            account_only: false,
            workspace: Some(WorkspaceInfo {
                id: uuid::Uuid::nil().to_string(),
                name: "Personal".into(),
            }),
            project: ProjectPlan::Create {
                name: "acme-web".into(),
                workspace_id: None,
                repository_url: None,
            },
            index: IndexPlan::Background,
        };
        print_review(&choices);

        let theme = ui::PromptTheme::new(*ui);
        let mut prompt = String::new();
        theme
            .format_select_prompt(&mut prompt, "Ready to set up ContextStream?")
            .unwrap();
        println!("{prompt}");
        for (index, (_, label)) in review_actions(IndexPlan::Background).iter().enumerate() {
            let mut item = String::new();
            theme
                .format_select_prompt_item(&mut item, label, index == 0)
                .unwrap();
            println!("{item}");
        }
        let mut answered = String::new();
        theme
            .format_select_prompt_selection(
                &mut answered,
                "Ready to set up ContextStream?",
                "Save and finish",
            )
            .unwrap();
        println!("{answered}");

        print_step(5, "Finishing setup", "");
        ui::say(Mark::Ok, "Project created", Some("acme-web"));
        ui::say(
            Mark::Ok,
            "Claude Code",
            Some("MCP global + project · rules global + project · hooks · sync"),
        );
        ui::say(Mark::Warn, "Cursor", Some("1 problem"));
        println!(
            "{}    {} ~/.cursor/mcp.json is locked by another process",
            ui::GUTTER,
            ui.mark(Mark::Fail)
        );
        ui::say(Mark::Ok, "Project linked", Some("~/code/acme-web"));
        ui::say(
            Mark::Ok,
            "Git capture on",
            Some("commits, pushes, checkouts, and merges"),
        );
        ui::say(
            Mark::Step,
            "Indexing in the background",
            Some("search fills in as it builds · you'll get a notification"),
        );
        ui::say(
            Mark::Ok,
            "Everything checks out",
            Some("your editor connects the next time it starts"),
        );

        let outcome = super::super::setup_completion_evidence(
            2, 2, true, true, true, true, false, true, false, false,
        );
        super::super::print_setup_outcome(
            "you@example.com",
            false,
            &[editors::Editor::ClaudeCode, editors::Editor::Cursor],
            choices.workspace.as_ref(),
            choices.project_path.as_deref(),
            &outcome,
        );
        println!(
            "{}",
            ui.status_bar(
                "ready",
                &[ui.faint("2 editors"), "main ✓".into()],
                "restart to connect"
            )
        );
    }

    #[test]
    fn display_path_abbreviates_home() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(display_path(&home), "~");
        assert_eq!(display_path(&home.join("code").join("app")), "~/code/app");
        assert_eq!(display_path(Path::new("/opt/app")), "/opt/app");
    }
}
