//! Asking before a config write changes the account an existing ContextStream
//! MCP entry signs in as.
//!
//! `update`, `setup` and every config refresh write the API key of the account
//! in HOME. An entry that already holds another account's key (a project
//! `.mcp.json` set up by someone else, or by the same person under another
//! HOME) would then sign in as the account in HOME without a word, and every
//! call from that folder would fail on the right account's workspaces. So
//! before a write puts a key for a different account in place of the one an
//! entry holds, or creates a project entry beside entries that sign in as
//! another account, this module says which file, the account it signs in as
//! now and the account it would switch to, by email, and asks (default No).
//! With no terminal, or under `setup --yes`, the entry stays as it is (its
//! key, installation id, binary, API URL and transport), a new entry is not
//! created, and it says how to do it on purpose: `--switch-account`, or setup
//! in a terminal (`mcp_config::settle_json_entry_account`).
//!
//! The same key, a new key for the same account and an entry with no key yet
//! are written as before, without a word. A key `/auth/me` does not resolve
//! (offline, refused, revoked) is "an unknown account", and a different key
//! then still counts as a switch. A key held by reference
//! (`${CONTEXTSTREAM_API_KEY}`, `{env:…}`) resolves to whoever runs the
//! editor, so it is the runner's own: it stays exactly as written, without a
//! word or a lookup, unless `--switch-account` asks for a literal key there.
//!
//! Each key goes only to the API it belongs to, with no redirects followed:
//! the key a write would put in is looked up only on this run's own API (its
//! saved credentials' URL, `CONTEXTSTREAM_API_URL` or the default), never on
//! anything an entry or a repository names, and an entry's key only on that
//! entry's own API. When the two differ there is no lookup at all, and the
//! keys count as different accounts.
//!
//! One exception: `configure` replacing the saved key passes that key to its
//! rotation refresh, and an entry holding exactly it is the same credential
//! being rotated. It takes the new key with no question, even when the API
//! now refuses the old one (`mcp_config::keys_to_settle_on_rotation`).

use std::path::{Path, PathBuf};
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

/// Whether `value` stands for a key the editor fills in when it starts
/// (`${CONTEXTSTREAM_API_KEY}`, `{env:CONTEXTSTREAM_API_KEY}`) rather than
/// being one.
pub(crate) fn is_key_reference(value: &str) -> bool {
    let value = value.trim();
    (value.starts_with("${") || value.starts_with("{env:")) && value.ends_with('}')
}

/// A key an entry holds, or would hold after a write: the value as written (a
/// literal key, or a reference the editor fills in) and the API URL the entry
/// sends it to, when that is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldKey {
    pub value: String,
    pub api_url: Option<String>,
}

impl HeldKey {
    pub(crate) fn new(value: impl Into<String>, api_url: Option<String>) -> Self {
        Self {
            value: value.into(),
            api_url,
        }
    }

    fn is_reference(&self) -> bool {
        is_key_reference(&self.value)
    }
}

/// What a project already signs in as, found beside an entry a write would
/// create: a key one of its configs holds, or an account its
/// `.contextstream/config.json` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectEvidence {
    pub path: PathBuf,
    pub held: Held,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    Key(HeldKey),
    Email(String),
}

/// The account a key signs in as, as far as this check can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Account {
    /// As the API's `/auth/me` names it.
    Email(String),
    /// `/auth/me` did not name one (offline, refused, revoked, a stub).
    Unknown,
    /// A key held by reference, which this check cannot resolve.
    Reference(String),
    /// A key whose entry talks to another API than the other key's (`None`
    /// when its entry does not say which): not looked up.
    OtherApi(Option<String>),
    /// The key this run would write, when `/auth/me` did not name its
    /// account or was not asked: this run's own account.
    Yours,
}

impl Account {
    fn label(&self) -> String {
        match self {
            Account::Email(email) => email.clone(),
            Account::Unknown | Account::OtherApi(None) => "an unknown account".to_string(),
            Account::Reference(reference) => format!("the key held as {reference}"),
            Account::OtherApi(Some(api_url)) => format!("an unknown account on {api_url}"),
            Account::Yours => "your account".to_string(),
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

/// A write that would switch the entry at `path` from one account to
/// another, or, with `creating`, create that project config beside `path`
/// signed in as another account than `path`.
pub(crate) struct Proposal<'a> {
    pub path: &'a Path,
    pub from: Account,
    pub to: Account,
    pub creating: Option<&'a Path>,
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
    /// The write would put a literal key where the entry holds one by
    /// reference, which is the runner's own: the reference stays exactly as
    /// written and the rest of the write applies, without a word.
    KeepReferences,
}

/// What a write may do with the keys of the entry at `path`, which holds
/// `current` keys now and would hold `proposed` ones. Asks or warns first
/// when it would put a key for another account in place of one it holds.
pub(crate) fn settle_key_write(path: &Path, current: &[HeldKey], proposed: &[HeldKey]) -> KeyWrite {
    let Some(new_key) = proposed
        .iter()
        .find(|key| !key.is_reference() && !current.iter().any(|held| held.value == key.value))
    else {
        return KeyWrite::NoSwitch;
    };
    if let Some(held) = current
        .iter()
        .find(|held| !held.is_reference() && held.value != new_key.value)
    {
        return match settle(path, held, new_key, None) {
            Settled::SameAccount => KeyWrite::NoSwitch,
            Settled::Switched => KeyWrite::Switched,
            Settled::Kept => KeyWrite::Keep(held.value.clone()),
        };
    }
    let Some(reference) = current.iter().find(|held| held.is_reference()) else {
        return KeyWrite::NoSwitch;
    };
    if mode() == Mode::Switch {
        let ui = ui::ui();
        eprintln!(
            "{}",
            ui.line(
                Mark::Info,
                &format!(
                    "Switching {} from the key held as {} to your account's key",
                    ui.path(&path.display().to_string()),
                    reference.value.trim()
                ),
                Some(SWITCH_ACCOUNT_FLAG),
            )
        );
        return KeyWrite::Switched;
    }
    KeyWrite::KeepReferences
}

/// The key a write must keep in the entry at `path` (see [`settle_key_write`]):
/// `Some` when the write would switch it to another account without
/// agreement, `None` when it may go ahead.
pub(crate) fn key_to_keep(
    path: &Path,
    current: &[HeldKey],
    proposed: &[HeldKey],
) -> Option<String> {
    match settle_key_write(path, current, proposed) {
        KeyWrite::Keep(key) => Some(key),
        KeyWrite::NoSwitch | KeyWrite::Switched | KeyWrite::KeepReferences => None,
    }
}

/// Whether a write may create the project config `new_path`, signed in with
/// `proposed`, in a project that already signs in as `evidence` (see
/// [`ProjectEvidence`]). A project with no ContextStream config has no
/// evidence and is created as before, and a key held by reference (the
/// runner's own) is no evidence.
pub(crate) fn settle_new_entry(
    new_path: &Path,
    evidence: &[ProjectEvidence],
    proposed: &HeldKey,
) -> Settled {
    for item in evidence {
        let settled = match &item.held {
            Held::Key(held) if held.is_reference() => Settled::SameAccount,
            Held::Key(held) => settle(&item.path, held, proposed, Some(new_path)),
            Held::Email(email) => {
                let from = Account::Email(email.clone());
                let to = account_of(proposed);
                if from.is_same(&to) {
                    Settled::SameAccount
                } else {
                    let proposal = Proposal {
                        path: &item.path,
                        from,
                        to,
                        creating: Some(new_path),
                    };
                    settled_by(decide(&proposal, mode(), ask, &mut |line| {
                        eprintln!("{line}")
                    }))
                }
            }
        };
        if settled != Settled::SameAccount {
            return settled;
        }
    }
    Settled::SameAccount
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

fn settled_by(choice: Choice) -> Settled {
    match choice {
        Choice::Switch => Settled::Switched,
        Choice::Keep => Settled::Kept,
    }
}

/// Whether a write may put `proposed` in place of `held` in the entry at
/// `path` (or, with `creating`, create that config beside it). Asks, warns or
/// says what it did when the keys sign in as different accounts (or either
/// account is unknown); keys never appear in what it says.
pub(crate) fn settle(
    path: &Path,
    held: &HeldKey,
    proposed: &HeldKey,
    creating: Option<&Path>,
) -> Settled {
    if held.value == proposed.value {
        return Settled::SameAccount;
    }
    let (from, to) = accounts_of(held, proposed);
    if from.is_same(&to) {
        return Settled::SameAccount;
    }
    let proposal = Proposal {
        path,
        from,
        to,
        creating,
    };
    settled_by(decide(&proposal, mode(), ask, &mut |line| {
        eprintln!("{line}")
    }))
}

/// The accounts two keys sign in as. Each is asked of the API its own entry
/// talks to, and only when both talk to the same one; a key held by
/// reference is never looked up.
fn accounts_of(held: &HeldKey, proposed: &HeldKey) -> (Account, Account) {
    let unlooked = |key: &HeldKey| {
        if key.is_reference() {
            Account::Reference(key.value.trim().to_string())
        } else {
            Account::OtherApi(key.api_url.clone())
        }
    };
    let api_url = match (&held.api_url, &proposed.api_url) {
        (Some(a), Some(b)) if same_api_url(a, b) => a.clone(),
        _ => return (unlooked(held), Account::Yours),
    };
    let lookups: Vec<String> = [held, proposed]
        .into_iter()
        .filter(|key| !key.is_reference())
        .map(|key| key.value.clone())
        .collect();
    let found = lookup_accounts(&api_url, &lookups);
    let account = |key: &HeldKey| {
        if key.is_reference() {
            return Account::Reference(key.value.trim().to_string());
        }
        lookups
            .iter()
            .position(|value| value == &key.value)
            .and_then(|index| found.get(index).cloned())
            .unwrap_or(Account::Unknown)
    };
    (account(held), yours(account(proposed)))
}

/// The key this run would write is this run's own account, named by email
/// when `/auth/me` did.
fn yours(account: Account) -> Account {
    match account {
        Account::Email(email) => Account::Email(email),
        _ => Account::Yours,
    }
}

/// The account this run's key signs in as, asked of this run's own API.
fn account_of(key: &HeldKey) -> Account {
    let Some(api_url) = key.api_url.as_deref().filter(|_| !key.is_reference()) else {
        return Account::Yours;
    };
    yours(
        lookup_accounts(api_url, std::slice::from_ref(&key.value))
            .pop()
            .unwrap_or(Account::Unknown),
    )
}

pub(crate) fn same_api_url(a: &str, b: &str) -> bool {
    let normalize = |url: &str| url.trim().trim_end_matches('/').to_ascii_lowercase();
    normalize(a) == normalize(b)
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
    let creating = proposal
        .creating
        .map(|creating| ui.path(&creating.display().to_string()));
    let (from, to) = (proposal.from.label(), proposal.to.label());
    if mode == Mode::Switch {
        let head = match &creating {
            Some(creating) => {
                format!("Creating {creating} for {to}, beside {path} for {from}")
            }
            None => format!("Switching {path} from {from} to {to}"),
        };
        say(ui.line(Mark::Info, &head, Some(SWITCH_ACCOUNT_FLAG)));
        return Choice::Switch;
    }
    let head = match &creating {
        Some(creating) => format!(
            "{path} signs in as {from}; creating {creating} would sign this project in as {to}."
        ),
        None => format!("{path} signs in as {from}; this would switch it to {to}."),
    };
    say(ui.line(Mark::Warn, &head, None));
    let kept = match (&creating, safe_edit::is_dry_run()) {
        (Some(creating), true) => format!("{GUTTER}  Would not create {creating}."),
        (Some(creating), false) => format!("{GUTTER}  Did not create {creating}."),
        (None, true) => {
            format!("{GUTTER}  Would keep its key, installation id, binary and API URL unchanged.")
        }
        (None, false) => {
            format!("{GUTTER}  Kept its key, installation id, binary and API URL unchanged.")
        }
    };
    if mode == Mode::Ask {
        let question = match &creating {
            Some(_) => format!("Create it for {to}?"),
            None => format!("Switch it to {to}?"),
        };
        // A question the person could not answer (Esc, Ctrl-C, a closed
        // terminal) is a No.
        if ask(&question).unwrap_or(false) {
            let done = match &creating {
                Some(creating) => format!("Creating {creating} for {to}"),
                None => format!("Switched {path} to {to}"),
            };
            say(ui.line(Mark::Ok, &done, None));
            return Choice::Switch;
        }
        say(kept);
        return Choice::Keep;
    }
    say(kept);
    let what = if creating.is_some() {
        "create it"
    } else {
        "switch it"
    };
    say(format!(
        "{GUTTER}  To {what} on purpose, run the command again with {SWITCH_ACCOUNT_FLAG}, \
         or run `contextstream-mcp setup` for it in a terminal and answer yes."
    ));
    Choice::Keep
}

/// Say that the entry at `path` keeps running `kept`, a binary in another
/// home than this one, where this write would have run `instead`.
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

/// The accounts `keys` sign in as on `api_url`, each looked up once per
/// process.
#[cfg(not(test))]
fn lookup_accounts(api_url: &str, keys: &[String]) -> Vec<Account> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static KNOWN: OnceLock<Mutex<HashMap<(String, String), Account>>> = OnceLock::new();
    let known = KNOWN.get_or_init(Default::default);
    let mut known = known.lock().unwrap_or_else(|error| error.into_inner());
    let slot = |key: &String| (api_url.trim_end_matches('/').to_string(), key.clone());
    let missing: Vec<String> = keys
        .iter()
        .filter(|key| !known.contains_key(&slot(key)))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let found = resolve_over_api(api_url, &missing);
        for (key, account) in missing.iter().zip(found) {
            known.insert(slot(key), account);
        }
    }
    keys.iter()
        .map(|key| known.get(&slot(key)).cloned().unwrap_or(Account::Unknown))
        .collect()
}

#[cfg(test)]
fn lookup_accounts(_api_url: &str, keys: &[String]) -> Vec<Account> {
    TEST_ACCOUNTS.with(|accounts| {
        let accounts = accounts.borrow();
        keys.iter()
            .map(|key| {
                accounts
                    .iter()
                    .find(|(known, _)| known == key)
                    .map(|(_, email)| Account::Email(email.clone()))
                    .unwrap_or(Account::Unknown)
            })
            .collect()
    })
}

/// Ask `<api_url>/api/v1/auth/me` whose each key is, following no redirect.
/// A key the API does not answer for (offline, refused, revoked, redirected,
/// no email) is an unknown account. The lookups run on their own thread and
/// runtime, so a synchronous config writer can make them from inside the
/// CLI's runtime.
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
            .redirect(reqwest::redirect::Policy::none())
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

    const API: &str = "https://api.example.test";

    fn key(value: &str) -> HeldKey {
        HeldKey::new(value, Some(API.to_string()))
    }

    fn keys(values: &[&str]) -> Vec<HeldKey> {
        values.iter().map(|value| key(value)).collect()
    }

    fn proposal<'a>(path: &'a Path, from: Account, to: Account) -> Proposal<'a> {
        Proposal {
            path,
            from,
            to,
            creating: None,
        }
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
                said.contains("Kept its key, installation id, binary and API URL unchanged."),
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
    fn creating_a_project_entry_beside_another_account_is_asked_and_otherwise_skipped() {
        let path = PathBuf::from("/work/repo/.mcp.json");
        let creating = PathBuf::from("/work/repo/.cursor/mcp.json");
        let proposal = Proposal {
            path: &path,
            from: erik(),
            to: chase(),
            creating: Some(&creating),
        };
        let mut lines = Vec::new();
        let choice = decide(&proposal, Mode::Keep, |_| Ok(true), &mut |line| {
            lines.push(line)
        });
        assert_eq!(choice, Choice::Keep);
        let said = lines.join("\n");
        assert!(
            said.contains(
                "/work/repo/.mcp.json signs in as erik@example.test; creating \
                 /work/repo/.cursor/mcp.json would sign this project in as chase@example.test."
            ),
            "{said}"
        );
        assert!(
            said.contains("Did not create /work/repo/.cursor/mcp.json."),
            "{said}"
        );
        assert!(said.contains("To create it on purpose"), "{said}");

        let mut asked = None;
        let choice = decide(
            &proposal,
            Mode::Ask,
            |question| {
                asked = Some(question.to_string());
                Ok(true)
            },
            &mut |_| {},
        );
        assert_eq!(choice, Choice::Switch);
        assert_eq!(asked.as_deref(), Some("Create it for chase@example.test?"));

        // Without a lookup, this run's own account is "your account".
        let proposal = Proposal {
            path: &path,
            from: Account::OtherApi(Some("http://127.0.0.1:9".to_string())),
            to: Account::Yours,
            creating: Some(&creating),
        };
        let mut asked = None;
        let mut lines = Vec::new();
        decide(
            &proposal,
            Mode::Ask,
            |question| {
                asked = Some(question.to_string());
                Ok(false)
            },
            &mut |line| lines.push(line),
        );
        assert_eq!(asked.as_deref(), Some("Create it for your account?"));
        assert!(
            lines[0].contains("would sign this project in as your account."),
            "{lines:?}"
        );
    }

    #[test]
    fn unknown_accounts_are_named_as_such() {
        let path = PathBuf::from("/home/someone/.claude.json");
        for (from, expected) in [
            (Account::Unknown, "signs in as an unknown account;"),
            (
                Account::Reference("${CONTEXTSTREAM_API_KEY}".to_string()),
                "signs in as the key held as ${CONTEXTSTREAM_API_KEY};",
            ),
            (
                Account::OtherApi(Some("http://127.0.0.1:9".to_string())),
                "signs in as an unknown account on http://127.0.0.1:9;",
            ),
        ] {
            let mut lines = Vec::new();
            decide(
                &proposal(&path, from, chase()),
                Mode::Keep,
                |_| Ok(true),
                &mut |line| lines.push(line),
            );
            assert!(lines[0].contains(expected), "{lines:?}");
        }
    }

    #[test]
    fn the_same_key_or_the_same_account_is_no_switch() {
        let _policy = test_policy(Mode::Keep, false);
        let path = Path::new("/work/repo/.mcp.json");
        assert_eq!(
            settle(path, &key("cs_key_a"), &key("cs_key_a"), None),
            Settled::SameAccount
        );

        let _accounts = test_accounts(&[
            ("cs_key_a", "Erik@Example.test"),
            ("cs_key_b", "erik@example.test"),
            ("cs_key_c", "chase@example.test"),
        ]);
        assert_eq!(
            settle(path, &key("cs_key_a"), &key("cs_key_b"), None),
            Settled::SameAccount
        );
        assert_eq!(
            settle(path, &key("cs_key_a"), &key("cs_key_c"), None),
            Settled::Kept
        );
        // A key nobody can resolve is an unknown account: a different key
        // still counts as a switch.
        assert_eq!(
            settle(path, &key("cs_unknown"), &key("cs_key_c"), None),
            Settled::Kept
        );
    }

    #[test]
    fn keys_on_different_apis_or_held_by_reference_are_never_the_same_account() {
        let _policy = test_policy(Mode::Keep, false);
        let _accounts = test_accounts(&[
            ("cs_key_a", "erik@example.test"),
            ("cs_key_b", "erik@example.test"),
        ]);
        let path = Path::new("/work/repo/.mcp.json");
        // The same account on paper, but the entries talk to different APIs:
        // a switch, decided without asking either API.
        assert_eq!(
            settle(
                path,
                &HeldKey::new("cs_key_a", Some("https://api.one.test".to_string())),
                &HeldKey::new("cs_key_b", Some("https://api.two.test/".to_string())),
                None
            ),
            Settled::Kept
        );
        // This run's key is never named from another API: it is "your
        // account".
        assert_eq!(
            accounts_of(
                &HeldKey::new("cs_key_a", Some("https://api.one.test".to_string())),
                &HeldKey::new("cs_key_b", Some("https://api.two.test".to_string())),
            ),
            (
                Account::OtherApi(Some("https://api.one.test".to_string())),
                Account::Yours
            )
        );
        // Trailing slashes and case do not make two APIs.
        assert_eq!(
            settle(
                path,
                &HeldKey::new("cs_key_a", Some("https://API.one.test/".to_string())),
                &HeldKey::new("cs_key_b", Some("https://api.one.test".to_string())),
                None
            ),
            Settled::SameAccount
        );
    }

    #[test]
    fn settle_key_write_tells_no_switch_an_agreed_switch_and_a_kept_key_apart() {
        let path = Path::new("/work/repo/.mcp.json");
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
    fn a_key_held_by_reference_is_the_runners_own_and_stays_unless_switched() {
        let path = Path::new("/work/repo/.mcp.json");
        let _accounts = test_accounts(&[("cs_chase", "chase@example.test")]);
        for reference in ["${CONTEXTSTREAM_API_KEY}", "{env:CONTEXTSTREAM_API_KEY}"] {
            let held = keys(&[reference]);
            // Left in place: nothing to settle.
            let _policy = test_policy(Mode::Keep, false);
            assert_eq!(settle_key_write(path, &held, &held), KeyWrite::NoSwitch);
            // A literal key in its place: kept silently, with or without a
            // terminal (no question is put)...
            assert_eq!(
                settle_key_write(path, &held, &keys(&["cs_chase"])),
                KeyWrite::KeepReferences
            );
            let _policy = test_policy(Mode::Ask, true);
            assert_eq!(
                settle_key_write(path, &held, &keys(&["cs_chase"])),
                KeyWrite::KeepReferences
            );
            // ...unless --switch-account asks for it.
            let _policy = test_policy(Mode::Switch, false);
            assert_eq!(
                settle_key_write(path, &held, &keys(&["cs_chase"])),
                KeyWrite::Switched
            );
        }
    }

    #[test]
    fn a_new_entry_is_settled_against_what_the_project_already_uses() {
        let _accounts = test_accounts(&[
            ("cs_erik", "erik@example.test"),
            ("cs_chase", "chase@example.test"),
        ]);
        let _policy = test_policy(Mode::Keep, false);
        let new_path = Path::new("/work/repo/.cursor/mcp.json");
        let evidence = |held: Held| {
            vec![ProjectEvidence {
                path: PathBuf::from("/work/repo/.mcp.json"),
                held,
            }]
        };
        assert_eq!(
            settle_new_entry(new_path, &[], &key("cs_chase")),
            Settled::SameAccount
        );
        // A key held by reference is the runner's own: no evidence.
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Key(key("${CONTEXTSTREAM_API_KEY}"))),
                &key("cs_chase")
            ),
            Settled::SameAccount
        );
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Key(key("cs_chase"))),
                &key("cs_chase")
            ),
            Settled::SameAccount
        );
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Key(key("cs_erik"))),
                &key("cs_chase")
            ),
            Settled::Kept
        );
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Email("Chase@example.test".to_string())),
                &key("cs_chase")
            ),
            Settled::SameAccount
        );
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Email("erik@example.test".to_string())),
                &key("cs_chase")
            ),
            Settled::Kept
        );
        let _policy = test_policy(Mode::Switch, false);
        assert_eq!(
            settle_new_entry(
                new_path,
                &evidence(Held::Key(key("cs_erik"))),
                &key("cs_chase")
            ),
            Settled::Switched
        );
    }

    #[test]
    fn key_to_keep_asks_only_when_a_held_key_would_be_replaced() {
        let _policy = test_policy(Mode::Keep, false);
        let path = Path::new("/work/repo/.mcp.json");
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
    fn key_references_are_recognized() {
        for reference in ["${CONTEXTSTREAM_API_KEY}", " {env:CONTEXTSTREAM_API_KEY} "] {
            assert!(is_key_reference(reference), "{reference}");
        }
        for literal in ["cs_live_key", "$CONTEXTSTREAM_API_KEY", "{cs}"] {
            assert!(!is_key_reference(literal), "{literal}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_me_names_each_key_follows_no_redirect_and_anything_else_is_unknown() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        use std::sync::{Arc, Mutex};

        // Where a redirect would lead: it must never be asked.
        let elsewhere_hits = Arc::new(Mutex::new(0usize));
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let elsewhere_url = format!("http://{}", elsewhere.local_addr().expect("addr"));
        let hits = Arc::clone(&elsewhere_hits);
        let elsewhere_app = Router::new().route(
            "/api/v1/auth/me",
            get(move || {
                *hits.lock().unwrap() += 1;
                async { Json(serde_json::json!({"data": {"email": "elsewhere@example.test"}})) }
            }),
        );
        let elsewhere_server = tokio::spawn(async move {
            let _ = axum::serve(elsewhere, elsewhere_app).await;
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let api_url = format!("http://{}", listener.local_addr().expect("addr"));
        let redirect_to = format!("{elsewhere_url}/api/v1/auth/me");
        let app = Router::new().route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| {
                let redirect_to = redirect_to.clone();
                async move {
                    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
                        Some("cs_erik") => {
                            Json(serde_json::json!({"data": {"email": "erik@example.test"}}))
                                .into_response()
                        }
                        Some("cs_chase") => {
                            Json(serde_json::json!({"data": {"email": "chase@example.test"}}))
                                .into_response()
                        }
                        Some("cs_moved") => (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(axum::http::header::LOCATION, redirect_to)],
                        )
                            .into_response(),
                        _ => (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({"error": "invalid api key"})),
                        )
                            .into_response(),
                    }
                }
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let keys = ["cs_erik", "cs_chase", "cs_revoked", "cs_moved"].map(String::from);
        // Called from inside a runtime, as the config writers are.
        let found = resolve_over_api(&format!("{api_url}/"), &keys);
        assert_eq!(
            found,
            vec![erik(), chase(), Account::Unknown, Account::Unknown]
        );
        assert_eq!(
            *elsewhere_hits.lock().unwrap(),
            0,
            "a redirect must not carry the key anywhere else"
        );

        // Nothing listening: every key is unknown, and the lookup still ends.
        server.abort();
        let _ = server.await;
        elsewhere_server.abort();
        let offline = resolve_over_api(&api_url, &keys[..1]);
        assert_eq!(offline, vec![Account::Unknown]);
    }
}
