//! Interactive `contextstream-mcp setup`.
//!
//! Sign in, then everything setup can decide safely is decided for the user:
//! detected editors, the folder's existing link or Git-repository match, the
//! only workspace, a new project named after the folder, and background
//! indexing. One review screen is the single confirmation and the place to
//! change any of those choices. Nothing on the server or on disk changes
//! before the user chooses Save.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mcp_client::ContextStreamClient;

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
    api_url: String,
    user_id: Option<String>,
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
    learning_state: super::account_learning::ConsentState,
    explicit_learning: bool,
    learning_choice: Option<bool>,
    /// `Some(true)` turns Claude Code auto memory off after saving; `None`
    /// when Claude Code is not being set up or auto memory is already off.
    claude_auto_memory_off: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewAction {
    Save,
    Editors,
    Project,
    Indexing,
    AccountLearning,
    ClaudeAutoMemory,
    Account,
    Exit,
}

fn learning_prompt_text(state: &super::account_learning::ConsentState) -> &'static str {
    if state.is_reconsent() {
        "Deep project learning terms were updated — open consent review after setup?"
    } else {
        "Open optional deep project learning consent after setup?"
    }
}

fn review_actions(
    index: IndexPlan,
    offer_learning_toggle: bool,
    claude_auto_memory_off: Option<bool>,
) -> Vec<(ReviewAction, &'static str)> {
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
    if offer_learning_toggle {
        actions.push((
            ReviewAction::AccountLearning,
            "Change consent-page reminder",
        ));
    }
    match claude_auto_memory_off {
        Some(true) => actions.push((ReviewAction::ClaudeAutoMemory, "Keep Claude auto memory on")),
        Some(false) => actions.push((
            ReviewAction::ClaudeAutoMemory,
            "Turn Claude auto memory off",
        )),
        None => {}
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
    explicit_learning_choice: Option<bool>,
) -> Result<bool> {
    prompts::require_terminal(
        "Guided setup",
        "Run `contextstream-mcp setup` in a terminal. For scripts, CI and coding agents, save a key \
         with `printf %s \"$KEY\" | contextstream-mcp configure --api-key-stdin` (or set \
         CONTEXTSTREAM_API_KEY), then run `contextstream-mcp setup --yes`.",
    )?;
    struct RestoreInstallationPersistence(bool);
    impl Drop for RestoreInstallationPersistence {
        fn drop(&mut self) {
            mcp_client::activation::set_installation_state_persistence_enabled(self.0);
        }
    }
    let persist_after_save = !safe_edit::is_dry_run();
    mcp_client::activation::set_installation_state_persistence_enabled(false);
    let _restore_installation_persistence = RestoreInstallationPersistence(persist_after_save);

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
    let (api_key, api_url, user_id, email) = sign_in(Some(signup_probe)).await?;
    let client = client_for(&api_key);
    let team_capable = load_team_capability(&client).await;
    let learning_state =
        super::account_learning::fetch_consent_state(&client, user_id.as_deref(), Some(&email))
            .await;

    print_step(
        2,
        "Connect your editors",
        "Each one gets the ContextStream MCP server, rules, and hooks.",
    );
    let detected = editors::detect_installed_editors_for_setup();
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
    let workspace = choose_workspace(&client, project_path.as_deref(), true, account_only).await?;
    let project = plan_project(&client, project_path.as_deref(), workspace.as_ref()).await?;
    let index = default_index_plan(project_path.as_deref(), &project);

    let learning_choice = match explicit_learning_choice {
        Some(choice) => Some(choice),
        None if learning_state.should_prompt_in_wizard() => Some(super::confirm(
            learning_prompt_text(&learning_state),
            false,
        )?),
        None => None,
    };

    let mut choices = Choices {
        api_key,
        api_url,
        user_id,
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
        learning_state,
        explicit_learning: explicit_learning_choice.is_some(),
        learning_choice,
        claude_auto_memory_off: None,
    };
    choices.claude_auto_memory_off = claude_auto_memory_default(&choices.editors);

    loop {
        print_step(
            4,
            "Review",
            "Nothing is saved until you choose Save. Change anything first.",
        );
        print_review(&choices);
        let offer_learning_toggle =
            !choices.explicit_learning && choices.learning_state.should_prompt_in_wizard();
        let actions = review_actions(
            choices.index,
            offer_learning_toggle,
            choices.claude_auto_memory_off,
        );
        let labels: Vec<&str> = actions.iter().map(|(_, label)| *label).collect();
        let choice = prompts::select("Ready to set up ContextStream?", &labels)?;
        match actions[choice].0 {
            ReviewAction::Save => break,
            ReviewAction::Editors => {
                choices.editors =
                    prompts::select_editors_with_defaults(&detected, &choices.editors)?;
                choices.transport = super::prompt_setup_transport_preference(&choices.editors)?;
                choices.claude_auto_memory_off = claude_auto_memory_default(&choices.editors);
            }
            ReviewAction::Project => {
                choices.workspace = choose_workspace(
                    &choices.client,
                    choices.project_path.as_deref(),
                    false,
                    false,
                )
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
            ReviewAction::AccountLearning => {
                choices.learning_choice = Some(super::confirm(
                    learning_prompt_text(&choices.learning_state),
                    false,
                )?);
            }
            ReviewAction::ClaudeAutoMemory => {
                choices.claude_auto_memory_off = choices.claude_auto_memory_off.map(|off| !off);
            }
            ReviewAction::Account => {
                let (api_key, api_url, user_id, email) = sign_in(None).await?;
                choices.client = client_for(&api_key);
                choices.api_key = api_key;
                choices.api_url = api_url;
                choices.user_id = user_id;
                choices.email = email;
                choices.team_capable = load_team_capability(&choices.client).await;
                choices.learning_state = super::account_learning::fetch_consent_state(
                    &choices.client,
                    choices.user_id.as_deref(),
                    Some(&choices.email),
                )
                .await;
                if !choices.explicit_learning {
                    choices.learning_choice = if choices.learning_state.should_prompt_in_wizard() {
                        Some(super::confirm(
                            learning_prompt_text(&choices.learning_state),
                            false,
                        )?)
                    } else {
                        None
                    };
                }
                choices.workspace = choose_workspace(
                    &choices.client,
                    choices.project_path.as_deref(),
                    true,
                    choices.account_only,
                )
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
                return Ok(false);
            }
        }
    }

    mcp_client::activation::set_installation_state_persistence_enabled(persist_after_save);
    print_step(5, "Finishing setup", "");
    let open_browser_after_finish = !choices.explicit_learning
        && choices.learning_state.should_prompt_in_wizard()
        && choices.learning_choice == Some(true);
    if !choices.explicit_learning
        && choices.learning_state.should_prompt_in_wizard()
        && choices.learning_choice == Some(false)
    {
        if let Err(error) = super::account_learning::record_declined(
            choices.user_id.as_deref(),
            Some(&choices.email),
            choices.learning_state.current_version(),
            choices.learning_state.is_reconsent(),
        ) {
            tracing::debug!(%error, "could not record local account-learning decline");
        }
    }
    let summary_hint = if choices.explicit_learning || open_browser_after_finish {
        super::account_learning::SummaryLearningHint::None
    } else {
        choices.learning_state.summary_hint()
    };
    finish(choices, summary_hint).await?;
    if open_browser_after_finish {
        super::account_learning::open_browser_consent_prompt();
    }
    Ok(true)
}

fn client_for(api_key: &str) -> ContextStreamClient {
    ContextStreamClient::new(super::setup_client_config(api_key))
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
async fn sign_in(
    signup_probe: Option<tokio::task::JoinHandle<bool>>,
) -> Result<(String, String, Option<String>, String)> {
    if safe_edit::is_dry_run() {
        let api_key = super::get_api_key_result()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Dry-run requires existing credentials because browser authentication would \
                 create server-side state. Authenticate once, then re-run --dry-run."
            )
        })?;
        let config = super::setup_client_config(&api_key);
        let api_url = config.api_url.clone();
        let user = ui::spin(
            "Checking your sign-in",
            ContextStreamClient::new(config).me(),
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!("Dry-run could not validate the existing credentials: {error}")
        })?;
        ui::say(Mark::Ok, "Signed in", Some(&user.email));
        return Ok((api_key, api_url, Some(user.id.to_string()), user.email));
    }

    // Only the first sign-in reuses env/saved credentials; switching accounts
    // from the review screen always signs in fresh. Credentials are persisted
    // only after the user chooses Save on the review screen.
    if signup_probe.is_some() {
        if let Some(api_key) = std::env::var("CONTEXTSTREAM_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
        {
            let config = super::setup_client_config(&api_key);
            let api_url = config.api_url.clone();
            let check = ui::spin(
                "Checking CONTEXTSTREAM_API_KEY",
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    ContextStreamClient::new(config).me(),
                ),
            )
            .await;
            match check {
                Ok(Ok(user)) => {
                    ui::say(Mark::Ok, "Signed in", Some(&user.email));
                    return Ok((api_key, api_url, Some(user.id.to_string()), user.email));
                }
                Ok(Err(error)) => ui::say(
                    Mark::Warn,
                    "CONTEXTSTREAM_API_KEY was rejected",
                    Some(&error.to_string()),
                ),
                Err(_) => ui::say(
                    Mark::Warn,
                    "Couldn't check CONTEXTSTREAM_API_KEY",
                    Some("the server didn't answer in time"),
                ),
            }
        } else if let Some(api_key) = super::read_saved_credentials()
            .ok()
            .and_then(|creds| creds.api_key)
            .filter(|key| !key.trim().is_empty())
        {
            let api_url = super::setup_client_config(&api_key).api_url;
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
                    return Ok((api_key, api_url, Some(user.id.to_string()), user.email));
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
    let config = super::setup_client_config(&api_key);
    let user_id =
        tokio::time::timeout(std::time::Duration::from_secs(5), client_for(&api_key).me())
            .await
            .ok()
            .and_then(Result::ok)
            .map(|user| user.id.to_string());
    ui::say(Mark::Ok, "Signed in", Some(&email));
    Ok((api_key, config.api_url, user_id, email))
}

fn initial_editors(
    only: Option<&[editors::Editor]>,
    detected: &[editors::Editor],
) -> Result<Vec<editors::Editor>> {
    if let Some(requested) = only {
        let deduped = super::dedupe_editors(requested.iter().copied());
        ui::say(
            Mark::Ok,
            "Editors",
            Some(&format!(
                "{} · from --editors",
                super::selected_editors_summary(&deduped)
            )),
        );
        super::warn_deprecated_editors(&deduped);
        return Ok(deduped);
    }
    let selection = mcp_client::activation::configured_client_selection()
        .context("Could not read the saved editor selection")?;
    let configured = super::dedupe_editors(
        selection
            .clients
            .iter()
            .filter_map(|id| editors::Editor::from_id(id)),
    );
    if !configured.is_empty() {
        ui::say(
            Mark::Ok,
            "Editors",
            Some(&format!(
                "{} · configured by setup",
                super::selected_editors_summary(&configured)
            )),
        );
        return Ok(configured);
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
    account_only: bool,
) -> Result<Option<WorkspaceInfo>> {
    let previous = project_path
        .filter(|_| !account_only)
        .and_then(|path| super::read_project_config(path).ok().flatten());
    let workspaces = ui::spin("Loading workspaces", client.list_workspaces(None, None)).await?;

    if auto {
        if account_only {
            return Ok(match workspaces.as_slice() {
                [only] => {
                    ui::say(Mark::Ok, "Workspace", Some(&only.name));
                    Some(WorkspaceInfo {
                        id: only.id.to_string(),
                        name: only.name.clone(),
                    })
                }
                _ => None,
            });
        }
        let linked = previous.as_ref().and_then(|config| {
            config.workspace_id.as_ref().and_then(|id| {
                workspaces
                    .iter()
                    .find(|workspace| workspace.id.to_string() == *id)
            })
        });
        let sibling_linked = if linked.is_none() {
            project_path
                .and_then(|path| super::sibling_checkout_workspace_id(path, &workspaces))
                .and_then(|id| workspaces.iter().find(|workspace| workspace.id == id))
        } else {
            None
        };
        let (chosen, reason) = match (linked, sibling_linked, workspaces.as_slice()) {
            (Some(workspace), _, _) => (Some(workspace), "used by this folder before"),
            (None, Some(workspace), _) => (Some(workspace), "used by a sibling checkout"),
            (None, None, [only]) => (Some(only), ""),
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

    let plan = decide_unambiguous_project_plan(&candidates);

    match plan {
        Some(plan) => {
            ui::say(Mark::Ok, "Project", Some(&plan.summary()));
            Ok(plan)
        }
        None => pick_project(client, project_path, Some(workspace)).await,
    }
}

fn decide_unambiguous_project_plan(candidates: &super::ProjectCandidates) -> Option<ProjectPlan> {
    if let Some(project) = candidates.linked_project.as_ref() {
        Some(ProjectPlan::Existing {
            project: ProjectInfo::from(project),
            reason: "linked to this folder",
        })
    } else if let [project] = candidates.repository_matches.as_slice() {
        Some(ProjectPlan::Existing {
            project: ProjectInfo::from(project),
            reason: "matches this Git repository",
        })
    } else if let [project] = candidates.sibling_checkout_matches.as_slice() {
        Some(ProjectPlan::Existing {
            project: ProjectInfo::from(project),
            reason: "linked to a sibling checkout",
        })
    } else if candidates.repository_matches.is_empty()
        && candidates.sibling_checkout_matches.is_empty()
        && candidates.folder_match.is_none()
    {
        Some(ProjectPlan::Create {
            name: candidates.folder_project_name.clone(),
            workspace_id: candidates.ws_id,
            repository_url: candidates.checkout_repository_url.clone(),
        })
    } else {
        None
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
    let resolved = super::resolve_setup_project_for_current_directory_with_overrides(
        client, path, workspace, true, true, true, None, None, true,
    )
    .await?;
    Ok(match resolved {
        super::SetupProjectResolution::Selected(project) => ProjectPlan::Existing {
            project,
            reason: "chosen",
        },
        super::SetupProjectResolution::PlannedCreateInDryRun {
            name,
            workspace_id,
            repository_url,
        } => ProjectPlan::Create {
            name,
            workspace_id,
            repository_url,
        },
        super::SetupProjectResolution::None => ProjectPlan::Unlinked,
    })
}

/// Default for the Claude auto memory choice: off whenever Claude Code is
/// being set up and auto memory is on (its system prompt otherwise saves
/// memories and preferences to local files), unless setup was told to keep
/// it. `None` when the choice does not apply. Review can change it.
fn claude_auto_memory_default(editors: &[editors::Editor]) -> Option<bool> {
    if !editors.contains(&editors::Editor::ClaudeCode) {
        return None;
    }
    if !super::hooks::claude_auto_memory_setting().is_some_and(|setting| setting.is_on()) {
        return None;
    }
    Some(!super::keep_claude_auto_memory())
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
    if choices.explicit_learning || choices.learning_state.should_prompt_in_wizard() {
        rows.push(ui.row(
            "Deep project learning",
            match choices.learning_choice {
                Some(true) => "Review consent in browser",
                Some(false) if choices.explicit_learning => "Withdraw consent after saving",
                Some(false) | None => "Not now (default) · turn on anytime later",
            },
        ));
    }
    if let Some(off) = choices.claude_auto_memory_off {
        rows.push(ui.row(
            "Claude auto memory",
            if off {
                "Off (default) · memories and preferences go to ContextStream"
            } else {
                "Keep on · Claude saves memories to local files"
            },
        ));
    }
    if matches!(choices.transport, SetupTransportPreference::LocalBinary) {
        rows.push(ui.row("Connection", &ui.warning("Local binary · recovery mode")));
    }
    println!("{}", ui.card(&rows));
    println!();
}

async fn finish(
    choices: Choices,
    learning_hint: super::account_learning::SummaryLearningHint,
) -> Result<()> {
    let Choices {
        api_key,
        api_url,
        user_id: _,
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
        learning_state: _,
        explicit_learning: _,
        learning_choice: _,
        claude_auto_memory_off,
    } = choices;

    let credentials_saved = super::persist_setup_credentials(&api_key, &api_url)?;
    if credentials_saved && !safe_edit::is_dry_run() {
        ui::say(
            Mark::Ok,
            "Credentials saved",
            Some(&display_path(&super::credentials_file_path())),
        );
    }

    let project_resolution = materialize_project(&client, project).await?;
    let selected_project = project_resolution.selected_project().cloned();
    if !project_resolution.has_project_target() {
        account_only = true;
    }
    let configured_project_path =
        project_resolution.configured_project_path(project_path.as_deref());

    super::persist_setup_editor_selection(&editors)?;

    let preauth = matches!(transport, SetupTransportPreference::HostedRemote)
        && editors
            .iter()
            .any(super::mcp_config::editor_supports_remote_mcp);
    let mut failed_editors = Vec::new();
    if !editors.is_empty() {
        // Persist transport intent before the first editor mutation so a
        // partial setup remains repairable with the same connection mode.
        super::write_setup_transport_marker(transport)?;
        let _dry_run_auto_memory_guard = super::hooks::setup_dry_run_auto_memory_off_guard(
            safe_edit::is_dry_run() && claude_auto_memory_off == Some(true),
        );
        for editor in &editors {
            // One editor failing (a locked or malformed config file) must not
            // stop the others; the failure is listed and repair is offered.
            if let Err(error) = super::configure_editor_with_workspace_and_project_name(
                &client,
                editor,
                &api_key,
                workspace.as_ref(),
                selected_project.as_ref().map(|p| p.id.as_str()),
                project_resolution.planned_project_name(),
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

    if claude_auto_memory_off == Some(true) {
        if safe_edit::is_dry_run() {
            ui::say(
                Mark::Info,
                "Would turn Claude auto memory off",
                Some("memories and preferences go to ContextStream"),
            );
        } else {
            if let Err(error) =
                mcp_client::activation::set_keep_claude_auto_memory_preference(false)
            {
                tracing::warn!(%error, "could not clear keep_claude_auto_memory preference");
            }
            match super::hooks::disable_claude_auto_memory() {
                Ok(_) => ui::say(
                    Mark::Ok,
                    "Claude auto memory off",
                    Some("memories and preferences go to ContextStream"),
                ),
                Err(error) => ui::say(
                    Mark::Warn,
                    "Claude auto memory is still on",
                    Some(&error.to_string()),
                ),
            }
        }
    } else if (claude_auto_memory_off == Some(false)
        || super::explicit_keep_claude_auto_memory_requested())
        && !safe_edit::is_dry_run()
    {
        if let Err(error) = mcp_client::activation::set_keep_claude_auto_memory_preference(true) {
            tracing::warn!(%error, "could not persist keep_claude_auto_memory preference");
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
            if safe_edit::is_dry_run() {
                ui::say(Mark::Info, "Would link project", Some(&display_path(path)));
            } else {
                ui::say(Mark::Ok, "Project linked", Some(&display_path(path)));
            }
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

    let doctor_healthy = if safe_edit::is_dry_run() {
        failed_editors.is_empty()
    } else {
        let report = ui::spin(
            "Checking everything works",
            super::doctor::build_report(configured_project_path, &editors),
        )
        .await;
        super::doctor::print_setup_health_report(&report);
        !report.has_setup_failures() && failed_editors.is_empty()
    };
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
        doctor_healthy,
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
        learning_hint,
    );
    Ok(())
}

async fn materialize_project(
    client: &ContextStreamClient,
    plan: ProjectPlan,
) -> Result<super::SetupProjectResolution> {
    match plan {
        ProjectPlan::Unlinked => Ok(super::SetupProjectResolution::None),
        ProjectPlan::Existing { project, .. } => {
            Ok(super::SetupProjectResolution::Selected(project))
        }
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
                return Ok(super::SetupProjectResolution::PlannedCreateInDryRun {
                    name,
                    workspace_id,
                    repository_url,
                });
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
            Ok(super::SetupProjectResolution::Selected(ProjectInfo {
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
        Ok(()) if safe_edit::is_dry_run() => ui::say(
            Mark::Info,
            "Would turn Git capture on",
            Some("commits, pushes, checkouts, and merges"),
        ),
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
    fn review_offers_the_claude_auto_memory_toggle_only_when_it_applies() {
        let label = |off| {
            review_actions(IndexPlan::Background, false, off)
                .into_iter()
                .find(|(action, _)| *action == ReviewAction::ClaudeAutoMemory)
                .map(|(_, label)| label)
        };
        assert_eq!(label(Some(true)), Some("Keep Claude auto memory on"));
        assert_eq!(label(Some(false)), Some("Turn Claude auto memory off"));
        assert_eq!(label(None), None);
        assert!(claude_auto_memory_default(&[editors::Editor::Cursor]).is_none());
    }

    #[test]
    fn review_offers_indexing_changes_only_when_there_is_something_to_index() {
        let with_index: Vec<_> = review_actions(IndexPlan::Background, false, None)
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        assert_eq!(with_index.first(), Some(&ReviewAction::Save));
        assert!(with_index.contains(&ReviewAction::Indexing));
        assert!(!with_index.contains(&ReviewAction::AccountLearning));
        assert_eq!(with_index.last(), Some(&ReviewAction::Exit));

        let empty: Vec<_> = review_actions(IndexPlan::NothingYet, true, None)
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        assert!(!empty.contains(&ReviewAction::Indexing));
        assert!(empty.contains(&ReviewAction::AccountLearning));
    }

    #[test]
    fn wizard_prompts_deep_learning_only_for_never_asked_or_reconsent() {
        use super::super::account_learning::ConsentState;

        for quiet_state in [
            ConsentState::On {
                version: Some("v1".into()),
            },
            ConsentState::Declined {
                version: Some("v1".into()),
            },
            ConsentState::Unreadable,
        ] {
            assert!(!quiet_state.should_prompt_in_wizard());
            let actions = review_actions(
                IndexPlan::Background,
                quiet_state.should_prompt_in_wizard(),
                None,
            );
            assert!(!actions
                .iter()
                .any(|(action, _)| *action == ReviewAction::AccountLearning));
        }

        let first_time = ConsentState::NeverAsked {
            version: Some("v1".into()),
        };
        assert!(first_time.should_prompt_in_wizard());
        assert_eq!(
            learning_prompt_text(&first_time),
            "Open optional deep project learning consent after setup?"
        );

        let reconsent = ConsentState::ReconsentNeeded {
            accepted_version: Some("v1".into()),
            current_version: Some("v2".into()),
        };
        assert!(reconsent.should_prompt_in_wizard());
        assert!(learning_prompt_text(&reconsent).contains("terms were updated"));
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
            api_url: String::new(),
            user_id: None,
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
            learning_state: super::super::account_learning::ConsentState::On {
                version: Some("account-learning-v1".into()),
            },
            explicit_learning: false,
            learning_choice: None,
            claude_auto_memory_off: None,
        };
        print_review(&choices);

        let theme = ui::PromptTheme::new(*ui);
        let mut prompt = String::new();
        theme
            .format_select_prompt(&mut prompt, "Ready to set up ContextStream?")
            .unwrap();
        println!("{prompt}");
        for (index, (_, label)) in review_actions(IndexPlan::Background, false, None)
            .iter()
            .enumerate()
        {
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
            super::super::account_learning::SummaryLearningHint::On,
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
    fn wizard_project_plan_reuses_unique_sibling_checkout_before_creating_worktree_duplicate() {
        let ws_id = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let proj_id = uuid::Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
        let sibling_project = mcp_types::api::Project {
            id: proj_id,
            name: "myapp".to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: Some(ws_id),
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: Some(1),
        };
        let candidates = super::super::ProjectCandidates {
            ws_id: Some(ws_id),
            folder_project_name: "myapp-worktree".to_string(),
            existing_projects: vec![sibling_project.clone()],
            checkout_repository_identity: None,
            checkout_repository_url: None,
            repository_matches: Vec::new(),
            sibling_checkout_matches: vec![sibling_project],
            linked_project: None,
            legacy_folder_matches: Vec::new(),
            folder_match: None,
        };
        let plan = decide_unambiguous_project_plan(&candidates).expect("unambiguous sibling plan");
        match plan {
            ProjectPlan::Existing { project, reason } => {
                assert_eq!(project.id, proj_id.to_string());
                assert_eq!(project.name, "myapp");
                assert_eq!(reason, "linked to a sibling checkout");
            }
            other => panic!("expected Existing sibling plan, got {:?}", other.summary()),
        }
    }

    #[test]
    fn display_path_abbreviates_home() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(display_path(&home), "~");
        assert_eq!(display_path(&home.join("code").join("app")), "~/code/app");
        assert_eq!(display_path(Path::new("/opt/app")), "/opt/app");
    }
}
