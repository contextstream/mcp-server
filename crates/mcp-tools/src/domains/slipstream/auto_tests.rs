//! Tests for automatic runs: which moments a request, command or edit is, which agent takes
//! each (from the live catalog's names and job lines), the input, the ledger, the setting,
//! and the hint end to end against a stub API.

use super::moments::{agent_for, command_moments, path_moments, prompt_moments, pull_number};
use super::*;
use mcp_types::Config;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";

fn workspace() -> Uuid {
    Uuid::parse_str(WORKSPACE).unwrap()
}

fn project() -> Uuid {
    Uuid::parse_str(PROJECT).unwrap()
}

/// The agents as the live API lists them (`GET .../tasks/agents/auto`): name, job line,
/// required inputs. Every one on and granted.
fn live_agents() -> Vec<AutoAgent> {
    let agent = |id: &str, name: &str, job: &str, inputs: &[&str]| AutoAgent {
        id: id.to_string(),
        name: name.to_string(),
        job: job.to_string(),
        enabled: true,
        scopes_ok: true,
        inputs_required: inputs.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        agent("analyst", "Data Analyst", "Answers a question about a CSV or XLSX table, in plain English or structured, with charts; every number is computed twice by the engine, never by the model.", &["data", "also_allow"]),
        agent("audit", "Security Audit", "Scans a public GitHub repo's lockfiles with osv-scanner, judges from the code whether each vulnerable package is actually used, and proposes an upgrade path for each.", &["repo_url"]),
        agent("ci-fix", "CI Fixer", "Reproduces a failing test run of a public GitHub repo in a sandbox and returns a minimal fix as a diff, with the passing test output.", &["repo_url"]),
        agent("dependency-upgrader", "Dependency Upgrader", "Upgrades a public GitHub repo within its semver ranges and runs its tests; when the upgrade breaks them, fixes the project's code.", &["repo_url"]),
        agent("docs-drift", "Docs Drift", "Finds statements in a repository's README and docs that the code no longer matches, and proposes the edits as a diff, each backed by the code that shows it.", &["project_id"]),
        agent("gardener", "Memory Gardener", "Finds duplicate or conflicting decisions and lessons, and stale todos, plans, tasks and reminders, and proposes merges, supersedes and closures for the person to approve.", &[]),
        agent("guide", "Repo Guide", "Writes an onboarding guide for an indexed repository: architecture, where things live, how to build, run and test it, with file:line citations and a diagram.", &["project_id"]),
        agent("keeper", "Plans & Tasks Keeper", "Answers status, what is next, what is blocked, who is on what and what changed, from the workspace's plans and tasks, in about 300 tokens citing their ids.", &["question"]),
        agent("lens", "Lens", "Reads an image you shared and ties it to your workspace: the text in it, related code, docs, decisions and lessons, and what to do next, with citations.", &["media_id"]),
        agent("mail", "Mail", "Reads your recent mail and writes a digest of what needs a reply, the commitments made and the deadlines coming, and saves reply drafts for you to send.", &[]),
        agent("oracle", "Code Oracle", "Answers where-is, what-calls, how-does-it-flow and where-to-change questions about indexed code: a cited answer of at most 600 tokens and a read list.", &["project_id", "question"]),
        agent("pdf", "PDF", "Makes a PDF from a doc, a plan, notes or markdown in the workspace, with contents, bookmarks and links, and stores it as media.", &[]),
        agent("release-notes", "Release Notes", "Turns the merged pull requests and commits between two refs into user-facing release notes, accounting for every merged pull request, and saves them as a release notes doc.", &["base", "head", "repo"]),
        agent("research-brief", "Research Brief", "Reads RSS and Atom feeds and their articles, then writes a short brief of the main stories in which every point carries a quote checked against the article.", &["sources"]),
        agent("review", "PR Reviewer", "Reviews a pull request's diff against the workspace's decisions, lessons and preferences, and returns findings that each cite a line of the diff and the constraint.", &["pull", "repo"]),
    ]
}

/// The same agents with the job lines the manifests' job statements give.
fn statement_agents() -> Vec<AutoAgent> {
    let jobs = [
        ("audit", "Security Audit finds known vulnerabilities in a public repository's locked dependencies and judges, citing the code, whether the project uses the vulnerable part of each."),
        ("ci-fix", "CI Fixer reproduces a failing test command in a public repository and returns the smallest change to the project's code that makes it pass, with the passing output."),
        ("dependency-upgrader", "Dependency Upgrader upgrades a public GitHub repository within its semver ranges, fixes the project's code when that breaks its tests, and returns the diff and the results."),
        ("docs-drift", "Docs Drift finds statements in a repository's docs that its code no longer matches and proposes the edits as a diff."),
        ("gardener", "The Memory Gardener finds duplicate or conflicting decisions and lessons and stale open items, and proposes merging, superseding or closing them."),
        ("guide", "Repo Guide writes an onboarding guide for an indexed repository, citing the files it describes."),
        ("oracle", "Code Oracle answers one question about where code is, what calls it, how it flows or where to change it, from the workspace's code index."),
        ("release-notes", "Release Notes writes user-facing release notes for the changes between two refs of a connected repository, from its merged pull requests and commits, and saves them as a doc."),
        ("review", "PR Reviewer reviews one pull request's diff for bugs and for breaches of the workspace's decisions, lessons and preferences, citing the line and the constraint for each finding."),
    ];
    live_agents()
        .into_iter()
        .map(|mut agent| {
            if let Some((_, job)) = jobs.iter().find(|(id, _)| *id == agent.id) {
                agent.job = job.to_string();
            }
            agent
        })
        .collect()
}

fn moment_ids(moments: &[&Moment]) -> Vec<String> {
    moments.iter().map(|m| m.id.clone()).collect()
}

fn agent_ids_for(prompt: &str, agents: &[AutoAgent]) -> Vec<String> {
    prompt_moments(prompt)
        .iter()
        .filter_map(|moment| agent_for(moment, agents))
        .map(|agent| agent.id.clone())
        .collect()
}

// ============================================================================
// Moments and agents
// ============================================================================

#[test]
fn the_table_parses_and_every_moment_has_cues() {
    let table = moments::table();
    assert_eq!(table.version, 1);
    assert!(table.moments.len() >= 9);
    for moment in &table.moments {
        assert!(!moment.why.is_empty(), "{}", moment.id);
        assert!(!moment.job.any_of.is_empty(), "{}", moment.id);
        assert!(
            !moment.prompt.any_of.is_empty() || !moment.prompt.phrases.is_empty(),
            "{}",
            moment.id
        );
    }
}

#[test]
fn ordinary_requests_fit_the_agent_that_does_that_work() {
    let cases = [
        (
            "upgrade the deps and make tests pass",
            "dependency-upgrader",
        ),
        (
            "Bump our dependencies to the latest minor versions",
            "dependency-upgrader",
        ),
        (
            "we have outdated packages, sort it out",
            "dependency-upgrader",
        ),
        ("we're about to release v1.2, get it ready", "release-notes"),
        ("Write the changelog for this sprint", "release-notes"),
        ("prepare the next release", "release-notes"),
        ("is lodash usage here vulnerable?", "audit"),
        ("check this project for known CVEs", "audit"),
        ("CI is failing on main, can you fix it", "ci-fix"),
        ("the tests are failing after the merge", "ci-fix"),
        (
            "Are the docs still accurate after this change?",
            "docs-drift",
        ),
        ("is the README up to date?", "docs-drift"),
        (
            "I'm new to this repo, give me an overview of this codebase",
            "guide",
        ),
        ("Where is the auth middleware defined?", "oracle"),
        ("how does the billing flow work?", "oracle"),
        ("where is parse_config used?", "oracle"),
        ("clean up stale decisions and todos", "gardener"),
        ("review PR #42", "review"),
        ("can you review pull request 17 in acme/api", "review"),
    ];
    for agents in [live_agents(), statement_agents()] {
        for (prompt, expected) in cases {
            assert_eq!(agent_ids_for(prompt, &agents), [expected], "{prompt}");
        }
    }
}

#[test]
fn ordinary_work_that_no_agent_should_take_fits_nothing() {
    for prompt in [
        "fix the null check in parser.rs",
        "run cargo build --release and tell me the size",
        "add a release button to the toolbar",
        "update the npm script for lint",
        "fix the wrong link in the docs",
        "where should I add the retry logic?",
        "how do I run the tests?",
        "release the lock before returning",
        "write a function that validates emails",
        "update package.json scripts to use vitest",
        "",
    ] {
        assert!(
            prompt_moments(prompt).is_empty(),
            "{prompt}: {:?}",
            moment_ids(&prompt_moments(prompt))
        );
    }
}

#[test]
fn a_request_that_says_not_to_do_it_fits_nothing() {
    for prompt in [
        "don't upgrade the dependencies, just fix the bug",
        "no need to update the packages",
        "do not run a security audit",
        "never bump the deps without asking",
    ] {
        assert!(
            prompt_moments(prompt).is_empty(),
            "{prompt}: {:?}",
            moment_ids(&prompt_moments(prompt))
        );
    }
    // A negation elsewhere in the request does not rule the moment out.
    assert_eq!(
        moment_ids(&prompt_moments(
            "upgrade the deps but don't touch the public API"
        )),
        ["dependencies"]
    );
    assert_eq!(
        moment_ids(&prompt_moments(
            "don't change the code yet: is lodash usage here vulnerable?"
        )),
        ["security"]
    );
}

#[test]
fn a_request_can_fit_two_moments() {
    assert_eq!(
        moment_ids(&prompt_moments(
            "upgrade the deps, then check for vulnerabilities"
        )),
        ["dependencies", "security"]
    );
}

#[test]
fn an_agent_that_is_off_or_needs_review_takes_nothing_and_no_other_agent_stands_in() {
    let mut agents = live_agents();
    for agent in &mut agents {
        if agent.id == "dependency-upgrader" {
            agent.enabled = false;
        }
        if agent.id == "audit" {
            agent.scopes_ok = false;
        }
    }
    assert!(agent_ids_for("upgrade the deps", &agents).is_empty());
    assert!(agent_ids_for("is lodash vulnerable?", &agents).is_empty());
    // Others still fit.
    assert_eq!(agent_ids_for("review PR #4", &agents), ["review"]);
}

#[test]
fn a_new_agent_fits_by_its_job_line_alone() {
    let agents = vec![AutoAgent {
        id: "upgrade-bot".to_string(),
        name: "Package Bumper".to_string(),
        job: "Upgrades a repository's dependencies and opens a pull request.".to_string(),
        enabled: true,
        scopes_ok: true,
        inputs_required: vec!["repo_url".to_string()],
    }];
    assert_eq!(agent_ids_for("upgrade the deps", &agents), ["upgrade-bot"]);
}

#[test]
fn commands_and_edits_are_moments() {
    let cases = [
        ("npm outdated", false, vec!["dependencies"]),
        (
            "FOO=1 sudo cargo update -p serde",
            false,
            vec!["dependencies"],
        ),
        (
            "bash -lc 'pip install -U requests'",
            false,
            vec!["dependencies"],
        ),
        ("npm audit --production", false, vec!["security"]),
        ("git tag v2.0.0", false, vec!["release"]),
        ("git tag -a v2.0.0 -m x", false, vec!["release"]),
        ("gh release create v2.0.0", false, vec!["release"]),
        ("gh run view 123 --log-failed", false, vec!["ci_failure"]),
        ("npm install", true, vec!["dependencies"]),
        ("gh pr checks", true, vec!["ci_failure"]),
    ];
    for (command, failed, expected) in cases {
        assert_eq!(
            moment_ids(&command_moments(command, failed)),
            expected,
            "{command}"
        );
    }
    for (command, failed) in [
        ("git tag", false),
        ("git tag --list", false),
        ("npm install", false),
        ("npm init", true),
        ("npm test", true),
        ("cargo build --release", false),
        ("echo npm outdated", false),
    ] {
        assert!(command_moments(command, failed).is_empty(), "{command}");
    }
    assert_eq!(moment_ids(&path_moments("/r/README.md")), ["docs"]);
    assert_eq!(moment_ids(&path_moments("docs/guide/setup.mdx")), ["docs"]);
    assert_eq!(moment_ids(&path_moments("C:\\r\\Cargo.lock")), ["security"]);
    assert_eq!(moment_ids(&path_moments("/r/CHANGELOG.md")), ["release"]);
    assert!(path_moments("/r/src/docs.rs").is_empty());
    assert!(path_moments("/r/docs/diagram.png").is_empty());
}

#[test]
fn pull_numbers_are_read_from_the_usual_forms() {
    assert_eq!(pull_number("review PR #42 please"), Some(42));
    assert_eq!(pull_number("review pr 7"), Some(7));
    assert_eq!(pull_number("review pull request 19"), Some(19));
    assert_eq!(
        pull_number("review https://github.com/acme/api/pull/314/files"),
        Some(314)
    );
    assert_eq!(pull_number("review my PR"), None);
}

// ============================================================================
// Input
// ============================================================================

#[test]
fn github_remotes_in_every_usual_form() {
    for url in [
        "https://github.com/acme/api.git",
        "https://github.com/acme/api",
        "https://x-access-token:secret@github.com/acme/api.git",
        "git@github.com:acme/api.git",
        "ssh://git@github.com/acme/api",
        "https://www.github.com/acme/api/",
    ] {
        assert_eq!(github_repo(url).as_deref(), Some("acme/api"), "{url}");
    }
    for url in [
        "https://gitlab.com/acme/api.git",
        "https://notgithub.com/acme/api",
        "/srv/git/api.git",
        "https://github.com/acme",
    ] {
        assert_eq!(github_repo(url), None, "{url}");
    }
}

fn agent(id: &str) -> AutoAgent {
    live_agents().into_iter().find(|a| a.id == id).unwrap()
}

#[test]
fn the_input_comes_from_the_repository_and_the_request() {
    let repo = RepoFacts {
        inspected: true,
        github: Some("acme/api".to_string()),
        default_branch: Some("main".to_string()),
        last_tag: Some("v1.1.0".to_string()),
        folder: None,
    };
    let (input, placeholders) = fill_input(
        &agent("dependency-upgrader"),
        Some("upgrade the deps"),
        &repo,
    )
    .unwrap();
    assert_eq!(
        Value::Object(input),
        json!({"repo_url": "https://github.com/acme/api"})
    );
    assert!(!placeholders);

    let (input, _) = fill_input(&agent("release-notes"), Some("release v1.2"), &repo).unwrap();
    assert_eq!(
        Value::Object(input),
        json!({"base": "v1.1.0", "head": "main", "repo": "acme/api"})
    );

    let (input, _) = fill_input(&agent("review"), Some("review PR #42"), &repo).unwrap();
    assert_eq!(
        Value::Object(input),
        json!({"pull": 42, "repo": "acme/api"})
    );
    // No pull request number: nothing to review.
    assert!(fill_input(&agent("review"), Some("review my PR"), &repo).is_none());

    let (input, _) = fill_input(
        &agent("oracle"),
        Some("  where is   the auth middleware defined?"),
        &repo,
    )
    .unwrap();
    assert_eq!(
        Value::Object(input),
        json!({"question": "where is the auth middleware defined?"})
    );

    // An input nobody can fill: no suggestion.
    assert!(fill_input(&agent("lens"), Some("what is in this image?"), &repo).is_none());
}

#[test]
fn a_repository_without_a_github_remote_gets_no_repository_agent_and_an_unknown_one_gets_a_placeholder(
) {
    let local_only = RepoFacts {
        inspected: true,
        ..RepoFacts::default()
    };
    assert!(fill_input(&agent("dependency-upgrader"), None, &local_only).is_none());
    let unknown = RepoFacts::default();
    let (input, placeholders) = fill_input(&agent("dependency-upgrader"), None, &unknown).unwrap();
    assert!(placeholders);
    assert_eq!(input["repo_url"], "<this repository's GitHub URL>");
    let (input, placeholders) = fill_input(&agent("release-notes"), None, &unknown).unwrap();
    assert!(placeholders);
    assert_eq!(input["base"], "<the last release tag>");
}

#[test]
fn a_checkout_folder_is_read_without_running_git() {
    let dir = std::env::temp_dir().join(format!("cs-auto-repo-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(
        dir.join(".git/config"),
        "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://github.com/other/fork\n[remote \"origin\"]\n\turl = git@github.com:acme/api.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
    )
    .unwrap();
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
    std::fs::write(
        dir.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/main\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let facts = RepoFacts::from_folder(dir.join("src").to_str().unwrap());
    assert!(facts.inspected);
    assert_eq!(facts.github.as_deref(), Some("acme/api"));
    assert_eq!(facts.default_branch.as_deref(), Some("main"));
    std::fs::remove_dir_all(&dir).unwrap();

    let missing = RepoFacts::from_folder("/definitely/not/a/folder/here");
    assert!(!missing.inspected);
}

// ============================================================================
// The setting
// ============================================================================

fn auto_answer(mcp: bool) -> Value {
    json!({"success": true, "data": {
        "auto_run": {"mcp": mcp, "contextcode": "off", "per_run_credit_limit": 2500},
        "blocked": null,
        "agents": serde_json::to_value(live_agents()).unwrap(),
    }})
}

#[test]
fn the_setting_is_read_as_the_api_answers_it() {
    let setting = AutoSetting::from_value(&auto_answer(true)).unwrap();
    assert!(setting.is_on());
    assert_eq!(setting.per_run_credit_limit, Some(2500));
    assert_eq!(setting.agents.len(), live_agents().len());
    assert!(setting.runnable("dependency-upgrader@1.0.0").is_some());
    assert!(!AutoSetting::from_value(&auto_answer(false))
        .unwrap()
        .is_on());

    let blocked = AutoSetting::from_value(&json!({
        "auto_run": {"mcp": true, "contextcode": "auto", "per_run_credit_limit": null},
        "blocked": {"code": "no_monthly_cap", "message": "Set a monthly credit cap first"},
        "agents": [{"id": "oracle", "enabled": true}]
    }))
    .unwrap();
    assert!(!blocked.is_on());
    assert_eq!(
        blocked.blocked.as_deref(),
        Some("Set a monthly credit cap first")
    );
    // An agent without scopes_ok is not runnable.
    assert!(blocked.runnable("oracle").is_none());

    assert!(AutoSetting::from_value(&json!({"agents": []})).is_none());
    assert_eq!(
        SettingRead::from_result(Err(Error::http(404, "Not Found"))),
        SettingRead::Unsupported
    );
    assert_eq!(
        SettingRead::from_result(Err(Error::http(503, "busy"))),
        SettingRead::Unreachable
    );
}

// ============================================================================
// The ledger
// ============================================================================

#[test]
fn each_agent_is_suggested_once_per_task_and_nothing_after_a_refusal() {
    let ledger = Ledger::in_memory();
    let (ws, p) = (workspace(), Some(project()));
    assert!(ledger.may_suggest("me", ws, p, "audit"));
    ledger.mark_suggested("me", ws, p, &["audit"]);
    assert!(!ledger.may_suggest("me", ws, p, "audit@0.1.0"));
    // Another project, another caller, another agent: still open.
    assert!(ledger.may_suggest("me", ws, None, "audit"));
    assert!(ledger.may_suggest("you", ws, p, "audit"));
    assert!(ledger.may_suggest("me", ws, p, "review"));

    let task = Uuid::new_v4();
    ledger.mark_started("me", ws, p, "review", task);
    assert!(!ledger.may_suggest("me", ws, p, "review"));
    assert_eq!(ledger.pending("me", ws).len(), 1);
    assert!(ledger.pending("you", ws).is_empty());
    ledger.reported(&[task]);
    assert!(ledger.pending("me", ws).is_empty());

    ledger.pause("me", ws);
    assert!(ledger.is_paused("me", ws));
    assert!(!ledger.may_suggest("me", ws, p, "gardener"));
    assert!(!ledger.is_paused("you", ws));
}

#[test]
fn the_ledger_on_disk_is_shared_between_processes() {
    let dir = std::env::temp_dir().join(format!("cs-auto-ledger-{}", Uuid::new_v4().simple()));
    let path = dir.join("ledger.json");
    let hook = Ledger::in_file(path.clone());
    hook.mark_suggested("me", workspace(), None, &["audit"]);
    let server = Ledger::in_file(path.clone());
    assert!(!server.may_suggest("me", workspace(), None, "audit"));
    server.pause("me", workspace());
    assert!(hook.is_paused("me", workspace()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "owner-only");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// ============================================================================
// The hint, end to end
// ============================================================================

#[test]
fn the_hint_names_the_exact_call_and_never_a_price() {
    let suggestion = Suggestion {
        agent: agent("dependency-upgrader"),
        why: "a dependency upgrade".to_string(),
        input: json!({"repo_url": "https://github.com/acme/api"})
            .as_object()
            .unwrap()
            .clone(),
        has_placeholders: false,
    };
    let text = render_hint(
        &[suggestion],
        Trigger::Request,
        "mcp__contextstream__agents",
    );
    assert!(text.starts_with("[CONTEXTAGENTS] Do this first:"), "{text}");
    assert!(text.contains(
        "- Dependency Upgrader (a dependency upgrade): Upgrades a public GitHub repo within its semver ranges"
    ));
    assert!(text.contains(
        "  mcp__contextstream__agents(action=\"run\", agent=\"dependency-upgrader\", input={\"repo_url\":\"https://github.com/acme/api\"}, automatic=true)\n"
    ));
    assert!(text.contains("without asking"));
    assert!(text.contains("get_run"));
    assert!(!text.contains("credit"));
    assert!(text.chars().count() < 900, "{}", text.chars().count());
}

/// A stub API that answers `/tasks/agents/auto` with `auto` and counts its reads.
async fn stub(auto: Value, status: u16) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let auto = auto.clone();
            let counted = counted.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16384];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                let (status, body) = if head.contains("/tasks/agents/auto") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    (status, auto.to_string())
                } else {
                    (
                        404,
                        json!({"success": false, "error": {"code": "NOT_FOUND", "message": "no"}})
                            .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, reads)
}

fn client_for(url: &str, key: &str) -> ContextStreamClient {
    ContextStreamClient::new(Config {
        api_url: url.to_string(),
        api_key: Some(key.to_string()),
        default_workspace_id: Some(workspace()),
        ..Config::default()
    })
}

async fn hint_for(client: &ContextStreamClient, prompt: &str) -> Option<String> {
    let caller = caller(client).await.unwrap();
    let scope = SuggestScope {
        client,
        caller: &caller,
        workspace_id: workspace(),
        project_id: Some(project()),
        shared: false,
        budget: Duration::from_secs(5),
    };
    let repo = RepoFacts {
        inspected: true,
        github: Some("acme/api".to_string()),
        ..RepoFacts::default()
    };
    suggest(
        &scope,
        &prompt_moments(prompt),
        Some(prompt),
        || repo,
        Trigger::Request,
        "agents",
    )
    .await
}

#[tokio::test]
async fn with_the_setting_on_a_fitting_request_gets_the_call_once() {
    let (url, reads) = stub(auto_answer(true), 200).await;
    let client = client_for(&url, &format!("key-on-{}", Uuid::new_v4()));
    let hint = hint_for(&client, "upgrade the deps and make tests pass")
        .await
        .expect("a hint");
    assert!(hint.contains(
        "agents(action=\"run\", agent=\"dependency-upgrader\", input={\"repo_url\":\"https://github.com/acme/api\"}, automatic=true)"
    ));
    // The same task again: not suggested twice. The setting was read once.
    assert!(hint_for(&client, "upgrade the deps please").await.is_none());
    // Another agent for the same task still is.
    assert!(hint_for(&client, "is lodash vulnerable?")
        .await
        .unwrap()
        .contains("agent=\"audit\""));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn with_the_setting_off_nothing_is_suggested_and_nothing_fitting_reads_nothing() {
    let (url, reads) = stub(auto_answer(false), 200).await;
    let client = client_for(&url, &format!("key-off-{}", Uuid::new_v4()));
    assert!(hint_for(&client, "fix the null check in parser.rs")
        .await
        .is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no moment, no read");
    assert!(hint_for(&client, "upgrade the deps").await.is_none());
    assert!(hint_for(&client, "is lodash vulnerable?").await.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 1, "kept for 60 s");
}

#[tokio::test]
async fn an_api_without_the_route_is_off() {
    let (url, reads) = stub(
        json!({"success": false, "error": {"code": "NOT_FOUND", "message": "Not Found"}}),
        404,
    )
    .await;
    let client = client_for(&url, &format!("key-old-{}", Uuid::new_v4()));
    assert!(hint_for(&client, "upgrade the deps").await.is_none());
    assert!(hint_for(&client, "review PR #3").await.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn after_a_refusal_nothing_is_suggested() {
    let (url, _) = stub(auto_answer(true), 200).await;
    let client = client_for(&url, &format!("key-paused-{}", Uuid::new_v4()));
    let me = caller(&client).await.unwrap();
    Ledger::for_process(false).pause(&me, workspace());
    assert!(hint_for(&client, "upgrade the deps").await.is_none());
}

#[test]
fn a_finished_run_is_reported_in_one_line() {
    let run = PendingRun {
        caller: "me".to_string(),
        workspace_id: workspace(),
        task_id: Uuid::nil(),
        agent: "audit".to_string(),
        started_at: 0,
    };
    let done = json!({"done": true, "status": "succeeded",
                      "output": {"summary": "Two vulnerable packages; lodash is not reachable."}});
    assert_eq!(
        finished_line(&run, &done, "agents").unwrap(),
        "- audit succeeded: Two vulnerable packages; lodash is not reachable. Full result: agents(action=\"get_run\", task_id=\"00000000-0000-0000-0000-000000000000\")."
    );
    let declined = json!({"done": true, "status": "declined",
                          "output": {"declined": {"reason": "Not a public GitHub repository."}}});
    assert!(finished_line(&run, &declined, "agents")
        .unwrap()
        .contains("declined: Not a public GitHub repository."));
    assert!(finished_line(&run, &json!({"done": false, "status": "running"}), "agents").is_none());
}
