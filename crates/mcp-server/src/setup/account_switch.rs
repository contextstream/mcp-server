//! Asking before a config write changes the account an existing ContextStream
//! MCP entry signs in as.
//!
//! `update`, `setup` and every config refresh write the API key of the account
//! in HOME. An entry that already holds another account's key (a project
//! `.mcp.json` set up by someone else, or by the same person under another
//! HOME) would then sign in as the account in HOME without a word, and every
//! call from that folder would fail on the right account's workspaces. So
//! before a write puts a key for a different account in place of the one an
//! entry holds, this module says which file, the account it signs in as now
//! and the account it would switch to, by email, and asks (default No). With
//! no terminal, or under `setup --yes`, it keeps the key and says how to switch
//! on purpose: `--switch-account`, or setup in a terminal. A kept entry also
//! keeps its installation id and the binary it runs
//! (`mcp_config::settle_json_entry_account`).
//!
//! The same key, a new key for the same account and an entry with no key yet
//! are written as before, without a word. A key `/auth/me` does not resolve
//! (offline, refused, revoked) is "an unknown account", and a different key
//! then still counts as a switch.
//!
//! One exception: `configure` replacing the saved key passes that key to its
//! rotation refresh, and an entry holding exactly it is the same credential
//! being rotated. It takes the new key with no question, even when the API
//! now refuses the old one (`mcp_config::keys_to_settle_on_rotation`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::safe_edit;
use super::ui::{self, Mark, GUTTER};

/// The flag that switches such entries to the account in HOME without asking.
pub const SWITCH_ACCOUNT_FLAG: &str = "--switch-account";

/// How long one `/auth/me` lookup may take before its account is unknown.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

static SWITCH_ALLOWED: AtomicBool = AtomicBool::new(false);
static QUESTIONS_ALLOWED: AtomicBool = AtomicBool::new(true);

/// `--switch-account`: switch entries that hold another account's key to the
/// account in HOME without asking.
pub fn allow_switch(allowed: bool) {
    SWITCH_ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Whether this run was given `--switch-account`.
pub fn switch_allowed() -> bool {
    SWITCH_ALLOWED.load(Ordering::Relaxed)
}

/// Runs that must not stop on a question (`setup --yes`, a setup profile,
/// `doctor --repair`) keep the key instead of asking, as without a terminal.
pub fn set_questions_allowed(allowed: bool) {
    QUESTIONS_ALLOWED.store(allowed, Ordering::Relaxed);
}

/// The account a key signs in as, as the API's `/auth/me` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Account {
    Email(String),
    Unknown,
}

impl Account {
    fn label(&self) -> &str {
        match self {
            Account::Email(email) => email,
            Account::Unknown => "an unknown account",
        }
    }

    fn is_same(&self, other: &Account) -> bool {
        matches!((self, other), (Account::Email(a), Account::Email(b)) if a.eq_ignore_ascii_case(b))
    }
}

/// How this run settles a switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `--switch-account`: switch, and say so.
    Switch,
    /// A terminal: ask, default No.
    Ask,
    /// No terminal, `--yes`, a dry run: keep the key and say how to switch.
    Keep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Switch,
    Keep,
}

/// A write that would switch the entry at `path` from one account to another.
pub(crate) struct Proposal<'a> {
    pub path: &'a Path,
    pub from: Account,
    pub to: Account,
}

/// How a write that holds `proposed` keys may replace an entry that holds
/// `current` ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyWrite {
    /// No key for another account goes in place of a held one: the same key,
    /// a new key for the same account, or an entry with no key yet.
    NoSwitch,
    /// The entry switches to another account, as agreed (`--switch-account`,
    /// or a yes on a terminal).
    Switched,
    /// The switch was not agreed: the entry keeps this key it holds, and the
    /// rest of what ties it to its own account and home.
    Keep(String),
}

/// What a write may do with the keys of the entry at `path`, which holds
/// `current` keys now and would hold `proposed` ones. Asks or warns first
/// when it would put a key for another account in place of one it holds.
pub(crate) fn settle_key_write(path: &Path, current: &[String], proposed: &[String]) -> KeyWrite {
    let Some(new_key) = proposed.iter().find(|key| !current.contains(key)) else {
        return KeyWrite::NoSwitch;
    };
    let Some(held) = current.iter().find(|key| *key != new_key) else {
        return KeyWrite::NoSwitch;
    };
    match settle(path, held, new_key) {
        Settled::SameAccount => KeyWrite::NoSwitch,
        Settled::Switched => KeyWrite::Switched,
        Settled::Kept => KeyWrite::Keep(held.clone()),
    }
}

/// The key a write must keep in the entry at `path` (see [`settle_key_write`]):
/// `Some` when the write would switch it to another account without
/// agreement, `None` when it may go ahead.
pub(crate) fn key_to_keep(path: &Path, current: &[String], proposed: &[String]) -> Option<String> {
    match settle_key_write(path, current, proposed) {
        KeyWrite::Keep(key) => Some(key),
        KeyWrite::NoSwitch | KeyWrite::Switched => None,
    }
}

/// How [`settle`] settled one key in place of another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The same key, or both keys sign in as the same account.
    SameAccount,
    /// Another account, and the switch was agreed.
    Switched,
    /// Another (or an unknown) account, and the switch was not agreed.
    Kept,
}

/// Whether a write may put `proposed` in place of `current` in the entry at
/// `path`. Asks, warns or says what it did when the keys sign in as different
/// accounts (or either account is unknown); keys never appear in what it says.
pub(crate) fn settle(path: &Path, current: &str, proposed: &str) -> Settled {
    if current == proposed {
        return Settled::SameAccount;
    }
    let (from, to) = accounts_of(current, proposed);
    if from.is_same(&to) {
        return Settled::SameAccount;
    }
    let proposal = Proposal { path, from, to };
    match decide(&proposal, mode(), ask, &mut |line| eprintln!("{line}")) {
        Choice::Switch => Settled::Switched,
        Choice::Keep => Settled::Kept,
    }
}

/// Say that the entry at `path` still runs `kept`, a binary from another home
/// than this one, where this write would have run `instead`.
pub(crate) fn say_binary_from_another_home(path: &Path, kept: &str, instead: &str) {
    eprintln!("{}", binary_from_another_home_line(path, kept, instead));
}

fn binary_from_another_home_line(path: &Path, kept: &str, instead: &str) -> String {
    let ui = ui::ui();
    ui.line(
        Mark::Warn,
        &format!(
            "{} runs {}, from another home than this one; it was not pointed at {}.",
            ui.path(&path.display().to_string()),
            ui.path(kept),
            ui.path(instead)
        ),
        None,
    )
}

/// What to do with `proposal` in `mode`, saying it line by line through `say`;
/// `ask` puts the question to a person and answers whether they said yes.
pub(crate) fn decide(
    proposal: &Proposal<'_>,
    mode: Mode,
    ask: impl FnOnce(&str) -> anyhow::Result<bool>,
    say: &mut dyn FnMut(String),
) -> Choice {
    let ui = ui::ui();
    let path = ui.path(&proposal.path.display().to_string());
    let (from, to) = (proposal.from.label(), proposal.to.label());
    if mode == Mode::Switch {
        say(ui.line(
            Mark::Info,
            &format!("Switching {path} from {from} to {to}"),
            Some(SWITCH_ACCOUNT_FLAG),
        ));
        return Choice::Switch;
    }
    say(ui.line(
        Mark::Warn,
        &format!("{path} signs in as {from}; this would switch it to {to}."),
        None,
    ));
    let kept = if safe_edit::is_dry_run() {
        format!("{GUTTER}  Would keep its key and installation id unchanged.")
    } else {
        format!("{GUTTER}  Kept its key and installation id unchanged.")
    };
    if mode == Mode::Ask {
        // A question the person could not answer (Esc, Ctrl-C, a closed
        // terminal) is a No.
        if ask(&format!("Switch it to {to}?")).unwrap_or(false) {
            say(ui.line(Mark::Ok, &format!("Switched {path} to {to}"), None));
            return Choice::Switch;
        }
        say(kept);
        return Choice::Keep;
    }
    say(kept);
    say(format!(
        "{GUTTER}  To switch it on purpose, run the command again with {SWITCH_ACCOUNT_FLAG}, \
         or run `contextstream-mcp setup` for it in a terminal and answer yes."
    ));
    Choice::Keep
}

fn mode() -> Mode {
    // Tests never read the developer's terminal: without a policy they run
    // as a script does.
    #[cfg(test)]
    return TEST_POLICY.get().map_or(Mode::Keep, |(mode, _)| mode);
    #[cfg(not(test))]
    {
        use std::io::IsTerminal;
        if switch_allowed() {
            return Mode::Switch;
        }
        let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        if terminal && QUESTIONS_ALLOWED.load(Ordering::Relaxed) && !safe_edit::is_dry_run() {
            Mode::Ask
        } else {
            Mode::Keep
        }
    }
}

#[cfg(not(test))]
fn ask(prompt: &str) -> anyhow::Result<bool> {
    super::prompts::confirm(prompt, false)
}

#[cfg(test)]
fn ask(_prompt: &str) -> anyhow::Result<bool> {
    Ok(TEST_POLICY.get().is_some_and(|(_, answer)| answer))
}

/// The accounts two keys sign in as, each looked up once per process.
#[cfg(not(test))]
fn accounts_of(current: &str, proposed: &str) -> (Account, Account) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static KNOWN: OnceLock<Mutex<HashMap<String, Account>>> = OnceLock::new();
    let known = KNOWN.get_or_init(Default::default);
    let mut known = known.lock().unwrap_or_else(|error| error.into_inner());
    let missing: Vec<String> = [current, proposed]
        .into_iter()
        .filter(|key| !known.contains_key(*key))
        .map(String::from)
        .collect();
    if !missing.is_empty() {
        let api_url = super::credentials::setup_client_config(proposed).api_url;
        let found = resolve_over_api(&api_url, &missing);
        known.extend(missing.into_iter().zip(found));
    }
    let account = |key: &str| known.get(key).cloned().unwrap_or(Account::Unknown);
    (account(current), account(proposed))
}

#[cfg(test)]
fn accounts_of(current: &str, proposed: &str) -> (Account, Account) {
    TEST_ACCOUNTS.with(|accounts| {
        let accounts = accounts.borrow();
        let account = |key: &str| {
            accounts
                .iter()
                .find(|(known, _)| known == key)
                .map(|(_, email)| Account::Email(email.clone()))
                .unwrap_or(Account::Unknown)
        };
        (account(current), account(proposed))
    })
}

/// Ask `<api_url>/api/v1/auth/me` whose each key is. A key the API does not
/// answer for (offline, refused, revoked, no email) is an unknown account.
/// The lookups run on their own thread and runtime, so a synchronous config
/// writer can make them from inside the CLI's runtime.
pub(crate) fn resolve_over_api(api_url: &str, keys: &[String]) -> Vec<Account> {
    let url = format!("{}/api/v1/auth/me", api_url.trim().trim_end_matches('/'));
    let lookup_keys = keys.to_vec();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let http = mcp_client::http::client_builder()
            .timeout(LOOKUP_TIMEOUT)
            .build()
            .ok()?;
        Some(
            runtime.block_on(futures::future::join_all(
                lookup_keys
                    .iter()
                    .map(|key| account_for_key(&http, &url, key)),
            )),
        )
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or_else(|| vec![Account::Unknown; keys.len()])
}

async fn account_for_key(http: &reqwest::Client, url: &str, key: &str) -> Account {
    let Ok(response) = http.get(url).header("X-API-Key", key).send().await else {
        return Account::Unknown;
    };
    if !response.status().is_success() {
        return Account::Unknown;
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return Account::Unknown;
    };
    body.get("data")
        .unwrap_or(&body)
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(|email| Account::Email(email.to_string()))
        .unwrap_or(Account::Unknown)
}

#[cfg(test)]
thread_local! {
    /// The mode and the person's answer the config writers on this test
    /// thread see; `None` is a run without a terminal.
    static TEST_POLICY: std::cell::Cell<Option<(Mode, bool)>> = const { std::cell::Cell::new(None) };
    /// The keys `/auth/me` would name on this test thread; any other key is
    /// an unknown account.
    static TEST_ACCOUNTS: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Until dropped, config writes on this test thread settle switches in `mode`,
/// and a question is answered `answer`.
#[cfg(test)]
pub(crate) fn test_policy(mode: Mode, answer: bool) -> TestGuard {
    TEST_POLICY.set(Some((mode, answer)));
    TestGuard
}

/// Until dropped, `/auth/me` names these `(key, email)` accounts on this test
/// thread.
#[cfg(test)]
pub(crate) fn test_accounts(accounts: &[(&str, &str)]) -> TestGuard {
    TEST_ACCOUNTS.with(|known| {
        *known.borrow_mut() = accounts
            .iter()
            .map(|(key, email)| (key.to_string(), email.to_string()))
            .collect();
    });
    TestGuard
}

#[cfg(test)]
pub(crate) struct TestGuard;

#[cfg(test)]
impl Drop for TestGuard {
    fn drop(&mut self) {
        TEST_POLICY.set(None);
        TEST_ACCOUNTS.with(|known| known.borrow_mut().clear());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proposal(path: &Path, from: Account, to: Account) -> Proposal<'_> {
        Proposal { path, from, to }
    }

    fn erik() -> Account {
        Account::Email("erik@example.test".to_string())
    }

    fn chase() -> Account {
        Account::Email("chase@example.test".to_string())
    }

    fn run(mode: Mode, answer: anyhow::Result<bool>) -> (Choice, Vec<String>, bool) {
        let path = PathBuf::from("/work/repo/.mcp.json");
        let mut lines = Vec::new();
        let mut asked = false;
        let choice = decide(
            &proposal(&path, erik(), chase()),
            mode,
            |prompt| {
                asked = true;
                assert_eq!(prompt, "Switch it to chase@example.test?");
                answer
            },
            &mut |line| lines.push(line),
        );
        (choice, lines, asked)
    }

    #[test]
    fn a_terminal_asks_and_a_yes_switches() {
        let (choice, lines, asked) = run(Mode::Ask, Ok(true));
        assert!(asked);
        assert_eq!(choice, Choice::Switch);
        let said = lines.join("\n");
        assert!(
            said.contains(
                "/work/repo/.mcp.json signs in as erik@example.test; this would switch it to chase@example.test."
            ),
            "{said}"
        );
        assert!(
            said.contains("Switched /work/repo/.mcp.json to chase@example.test"),
            "{said}"
        );
    }

    #[test]
    fn a_terminal_asks_and_a_no_or_no_answer_keeps_the_key() {
        for answer in [Ok(false), Err(anyhow::anyhow!("the terminal went away"))] {
            let (choice, lines, asked) = run(Mode::Ask, answer);
            assert!(asked);
            assert_eq!(choice, Choice::Keep);
            let said = lines.join("\n");
            assert!(
                said.contains("Kept its key and installation id unchanged."),
                "{said}"
            );
            assert!(!said.contains(SWITCH_ACCOUNT_FLAG), "{said}");
        }
    }

    #[test]
    fn without_a_terminal_it_keeps_the_key_and_says_how_to_switch() {
        let (choice, lines, asked) = run(Mode::Keep, Ok(true));
        assert!(!asked, "nothing is asked without a terminal");
        assert_eq!(choice, Choice::Keep);
        let said = lines.join("\n");
        assert!(said.contains("signs in as erik@example.test"), "{said}");
        assert!(said.contains("switch it to chase@example.test"), "{said}");
        assert!(said.contains("--switch-account"), "{said}");
        assert!(said.contains("contextstream-mcp setup"), "{said}");
    }

    #[test]
    fn switch_account_switches_without_asking_and_says_so() {
        let (choice, lines, asked) = run(Mode::Switch, Ok(false));
        assert!(!asked);
        assert_eq!(choice, Choice::Switch);
        assert!(lines.join("\n").contains(
            "Switching /work/repo/.mcp.json from erik@example.test to chase@example.test"
        ));
    }

    #[test]
    fn an_unknown_account_is_named_as_such() {
        let path = PathBuf::from("/home/someone/.claude.json");
        let mut lines = Vec::new();
        decide(
            &proposal(&path, Account::Unknown, chase()),
            Mode::Keep,
            |_| Ok(true),
            &mut |line| lines.push(line),
        );
        assert!(
            lines[0].contains("signs in as an unknown account"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_same_key_or_the_same_account_is_no_switch() {
        let _policy = test_policy(Mode::Keep, false);
        let path = Path::new("/work/repo/.mcp.json");
        assert_eq!(settle(path, "cs_key_a", "cs_key_a"), Settled::SameAccount);

        let _accounts = test_accounts(&[
            ("cs_key_a", "Erik@Example.test"),
            ("cs_key_b", "erik@example.test"),
            ("cs_key_c", "chase@example.test"),
        ]);
        assert_eq!(settle(path, "cs_key_a", "cs_key_b"), Settled::SameAccount);
        assert_eq!(settle(path, "cs_key_a", "cs_key_c"), Settled::Kept);
        // A key nobody can resolve is an unknown account: a different key
        // still counts as a switch.
        assert_eq!(settle(path, "cs_unknown", "cs_key_c"), Settled::Kept);
    }

    #[test]
    fn settle_key_write_tells_no_switch_an_agreed_switch_and_a_kept_key_apart() {
        let path = Path::new("/work/repo/.mcp.json");
        let keys = |list: &[&str]| list.iter().map(|key| key.to_string()).collect::<Vec<_>>();
        let _accounts = test_accounts(&[
            ("cs_erik", "erik@example.test"),
            ("cs_erik_new", "erik@example.test"),
            ("cs_chase", "chase@example.test"),
        ]);
        let _policy = test_policy(Mode::Keep, false);
        assert_eq!(
            settle_key_write(path, &keys(&["cs_erik"]), &keys(&["cs_erik_new"])),
            KeyWrite::NoSwitch
        );
        assert_eq!(
            settle_key_write(path, &[], &keys(&["cs_chase"])),
            KeyWrite::NoSwitch
        );
        assert_eq!(
            settle_key_write(path, &keys(&["cs_erik"]), &keys(&["cs_chase"])),
            KeyWrite::Keep("cs_erik".to_string())
        );
        let _policy = test_policy(Mode::Switch, false);
        assert_eq!(
            settle_key_write(path, &keys(&["cs_erik"]), &keys(&["cs_chase"])),
            KeyWrite::Switched
        );
    }

    #[test]
    fn the_other_home_warning_names_both_binaries() {
        let line = binary_from_another_home_line(
            Path::new("/work/repo/.mcp.json"),
            "/home/erik/.contextstream/bin/contextstream-mcp",
            "/home/chase/.contextstream/bin/contextstream-mcp",
        );
        assert!(
            line.contains(
                "/work/repo/.mcp.json runs /home/erik/.contextstream/bin/contextstream-mcp, from \
                 another home than this one; it was not pointed at \
                 /home/chase/.contextstream/bin/contextstream-mcp."
            ),
            "{line}"
        );
    }

    #[test]
    fn key_to_keep_asks_only_when_a_held_key_would_be_replaced() {
        let _policy = test_policy(Mode::Keep, false);
        let path = Path::new("/work/repo/.mcp.json");
        let keys = |list: &[&str]| list.iter().map(|key| key.to_string()).collect::<Vec<_>>();
        // No key yet, or no new key written: nothing to ask.
        assert_eq!(key_to_keep(path, &[], &keys(&["cs_b"])), None);
        assert_eq!(key_to_keep(path, &keys(&["cs_a"]), &[]), None);
        assert_eq!(key_to_keep(path, &keys(&["cs_a"]), &keys(&["cs_a"])), None);
        // Another key in place of the held one: kept without a terminal.
        assert_eq!(
            key_to_keep(path, &keys(&["cs_a"]), &keys(&["cs_b"])),
            Some("cs_a".to_string())
        );
        drop(_policy);
        let _policy = test_policy(Mode::Ask, true);
        assert_eq!(key_to_keep(path, &keys(&["cs_a"]), &keys(&["cs_b"])), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_me_names_each_key_and_anything_else_is_unknown() {
        use axum::{http::HeaderMap, http::StatusCode, routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let api_url = format!("http://{}", listener.local_addr().expect("addr"));
        let app = Router::new().route(
            "/api/v1/auth/me",
            get(|headers: HeaderMap| async move {
                let email = match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
                    Some("cs_erik") => "erik@example.test",
                    Some("cs_chase") => "chase@example.test",
                    _ => {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({"error": "invalid api key"})),
                        )
                    }
                };
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"data": {"email": email}})),
                )
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let keys = ["cs_erik", "cs_chase", "cs_revoked"].map(String::from);
        // Called from inside a runtime, as the config writers are.
        let found = resolve_over_api(&format!("{api_url}/"), &keys);
        assert_eq!(found, vec![erik(), chase(), Account::Unknown]);

        // Nothing listening: every key is unknown, and the lookup still ends.
        server.abort();
        let _ = server.await;
        let offline = resolve_over_api(&api_url, &keys[..1]);
        assert_eq!(offline, vec![Account::Unknown]);
    }
}
