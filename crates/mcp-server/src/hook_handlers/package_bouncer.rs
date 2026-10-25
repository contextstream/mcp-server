//! Package Bouncer: a PreToolUse hook on shell commands that checks the packages an install
//! command names against their public registry before the command runs.
//!
//! Coding agents sometimes install a package whose name they made up, or one a name away from
//! a popular package; an attacker who registers that name gets code run on the machine. The
//! bouncer reads the command (`npm install`, `pnpm add`, `yarn add`, `bun add`, `npx`, `pip
//! install`, `uv add`, `poetry add`, `uvx`, `cargo add`, `cargo install`, and the same inside
//! `claude mcp add ... -- npx ...`) and asks each package's registry about it:
//!
//! - a package the public registry does not have is denied, with the closest popular name;
//! - a package that exists but looks like a popular one and is barely downloaded is asked
//!   about, so the person decides;
//! - anything else, and any lookup that fails or runs out of time, is allowed.
//!
//! It never blocks on its own failure, skips a project or command that names a private
//! registry, and `CONTEXTSTREAM_PACKAGE_BOUNCER=off` turns it off. Supports Claude Code
//! (`PreToolUse` on `Bash`) and Cursor (`beforeShellExecution`).

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{input_is_cursor, read_stdin_json, write_stdout_json, HookOutput, HookSpecificOutput};

/// The whole check's budget, inside the hook's own timeout.
const CHECK_BUDGET: Duration = Duration::from_millis(3_500);
/// At most this many packages are looked up per command; the rest pass.
const MAX_PACKAGES: usize = 8;
/// Below this many weekly downloads a lookalike of a popular package is asked about.
const FEW_DOWNLOADS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Ecosystem {
    Npm,
    PyPi,
    Crates,
}

impl Ecosystem {
    fn registry(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::PyPi => "PyPI",
            Self::Crates => "crates.io",
        }
    }
}

/// A package an install command names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requested {
    pub ecosystem: Ecosystem,
    pub name: String,
}

/// What a registry says about a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    Missing,
    /// It exists; weekly downloads when the registry said.
    Found {
        weekly_downloads: Option<u64>,
    },
    /// The lookup failed or timed out: nothing is known.
    Unknown,
}

#[async_trait]
pub(crate) trait Registry: Send + Sync {
    async fn lookup(&self, package: &Requested) -> Lookup;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    Ask(String),
    Deny(String),
}

pub async fn handle() -> Result<()> {
    let input = read_stdin_json().unwrap_or(Value::Null);
    let cursor = input_is_cursor(&input);
    let verdict = if disabled() {
        Verdict::Allow
    } else {
        match shell_command(&input) {
            Some(command) => {
                let cwd = input
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .or_else(|| std::env::current_dir().ok());
                let requested =
                    requested_packages(&command, cwd.as_deref(), &Environment::current());
                check(&requested, &HttpRegistry::new(), CHECK_BUDGET).await
            }
            None => Verdict::Allow,
        }
    };
    if cursor {
        let output = match &verdict {
            Verdict::Allow => serde_json::json!({ "permission": "allow" }),
            Verdict::Ask(msg) => {
                serde_json::json!({ "permission": "ask", "user_message": msg, "agent_message": msg })
            }
            Verdict::Deny(msg) => {
                serde_json::json!({ "permission": "deny", "user_message": msg, "agent_message": msg })
            }
        };
        println!("{output}");
        return Ok(());
    }
    match verdict {
        Verdict::Allow => write_stdout_json(&HookOutput::empty()),
        Verdict::Deny(msg) => write_stdout_json(&HookOutput::deny_pre_tool_use(msg)),
        Verdict::Ask(msg) => write_stdout_json(&HookOutput {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: Some("PreToolUse".to_string()),
                additional_context: None,
                permission_decision: Some("ask".to_string()),
                permission_decision_reason: Some(msg),
            }),
            ..HookOutput::empty()
        }),
    }
}

fn disabled() -> bool {
    matches!(
        std::env::var("CONTEXTSTREAM_PACKAGE_BOUNCER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// The shell command a hook payload is about: Claude Code's Bash tool or Cursor's shell hook.
fn shell_command(input: &Value) -> Option<String> {
    if let Some(tool) = input.get("tool_name").and_then(Value::as_str) {
        if tool != "Bash" {
            return None;
        }
        return input
            .pointer("/tool_input/command")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    input
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Decides on a command's packages: deny one the registry lacks, ask about a barely used
/// lookalike, allow the rest. Lookups run at once, inside the budget.
pub(crate) async fn check(
    requested: &[Requested],
    registry: &dyn Registry,
    budget: Duration,
) -> Verdict {
    if requested.is_empty() {
        return Verdict::Allow;
    }
    let lookups = requested
        .iter()
        .take(MAX_PACKAGES)
        .map(|package| registry.lookup(package));
    let results = match tokio::time::timeout(budget, futures::future::join_all(lookups)).await {
        Ok(results) => results,
        Err(_) => return Verdict::Allow,
    };
    let mut missing = Vec::new();
    let mut doubtful = Vec::new();
    for (package, lookup) in requested.iter().zip(results) {
        let popular = lookalike_of(package);
        match lookup {
            Lookup::Missing => missing.push(match popular {
                Some(popular) => format!(
                    "`{}` is not on {} (did you mean `{popular}`?)",
                    package.name,
                    package.ecosystem.registry()
                ),
                None => format!(
                    "`{}` is not on {}",
                    package.name,
                    package.ecosystem.registry()
                ),
            }),
            Lookup::Found {
                weekly_downloads: Some(downloads),
            } if downloads < FEW_DOWNLOADS => {
                if let Some(popular) = popular {
                    doubtful.push(format!(
                        "`{}` on {} is a name away from the popular `{popular}` and had {downloads} downloads last week",
                        package.name,
                        package.ecosystem.registry()
                    ));
                }
            }
            _ => {}
        }
    }
    if !missing.is_empty() {
        return Verdict::Deny(format!(
            "Package Bouncer stopped this install: {}. A coding agent may have made the name up, and anyone can register a missing name with harmful code. Check the name; if the package comes from a private registry, configure it in .npmrc or pip.conf (the bouncer skips private registries), or set CONTEXTSTREAM_PACKAGE_BOUNCER=off.",
            missing.join("; ")
        ));
    }
    if !doubtful.is_empty() {
        return Verdict::Ask(format!(
            "Package Bouncer: {}. Lookalike names are a common way to slip in harmful code. Install it only if you meant this exact package.",
            doubtful.join("; ")
        ));
    }
    Verdict::Allow
}

// -- Reading install commands ---------------------------------------------------------------

/// Splits a command into words, honouring single and double quotes and backslashes, with
/// `&&`, `||`, `;`, `|` and newlines as their own words.
pub(crate) fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        word.push(n);
                    }
                }
            }
            '&' if in_word && word.ends_with('>') => word.push(c),
            ';' | '|' | '&' | '\n' | '(' | ')' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                let mut op = c.to_string();
                if (c == '&' || c == '|') && chars.peek() == Some(&c) {
                    op.push(chars.next().unwrap_or(c));
                }
                words.push(op);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

fn is_separator(word: &str) -> bool {
    matches!(word, ";" | "|" | "||" | "&" | "&&" | "\n" | "(" | ")")
}

/// What the private-registry checks read beyond the project: the user's home, the
/// environment and the system pip.conf.
pub(crate) struct Environment {
    pub home: Option<PathBuf>,
    pub vars: Vec<(String, String)>,
    pub system_pip_conf: Option<PathBuf>,
}

impl Environment {
    fn current() -> Self {
        Self {
            home: dirs::home_dir(),
            vars: std::env::vars().collect(),
            system_pip_conf: Some(PathBuf::from("/etc/pip.conf")),
        }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.trim().is_empty())
    }
}

/// The packages a command would install from a public registry. A command that names a
/// registry of its own, and a project configured for a private one, name none.
pub(crate) fn requested_packages(
    command: &str,
    cwd: Option<&Path>,
    env: &Environment,
) -> Vec<Requested> {
    let words = shell_words(command);
    let mut found = Vec::new();
    for segment in words.split(|w| is_separator(w)) {
        found.extend(segment_packages(segment));
    }
    let npm_private =
        found.iter().any(|p| p.ecosystem == Ecosystem::Npm) && npm_registry_is_private(cwd, env);
    let pypi_private =
        found.iter().any(|p| p.ecosystem == Ecosystem::PyPi) && python_index_is_private(cwd, env);
    found.retain(|p| match p.ecosystem {
        Ecosystem::Npm => !npm_private,
        Ecosystem::PyPi => !pypi_private,
        Ecosystem::Crates => true,
    });
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert((p.ecosystem, p.name.clone())));
    found
}

/// Drops redirections: `> file` and its target, `>file`, `2>&1`.
fn without_redirections(words: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip_target = false;
    for word in words {
        if skip_target {
            skip_target = false;
            continue;
        }
        let op = word.trim_start_matches(|c: char| c.is_ascii_digit());
        if op.starts_with('>') || op.starts_with('<') {
            skip_target = op.trim_start_matches(['>', '<', '&']).is_empty();
            continue;
        }
        out.push(word.as_str());
    }
    out
}

/// Where the command itself starts, past variable assignments and `sudo`, `env` and the like.
fn command_start(w: &[&str]) -> usize {
    let mut i = 0;
    while let Some(word) = w.get(i) {
        let assignment = word.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        let prefix = matches!(
            *word,
            "sudo" | "doas" | "env" | "time" | "nohup" | "exec" | "command"
        ) || (i > 0 && word.starts_with('-'));
        if !(assignment || prefix) {
            break;
        }
        i += 1;
    }
    i
}

/// The packages one simple command installs. The installer is read where the command starts
/// and after a `--`, so `claude mcp add x -- npx -y pkg` is read too, and `echo npm i x` is not.
fn segment_packages(words: &[String]) -> Vec<Requested> {
    let w = without_redirections(words);
    let mut starts = vec![command_start(&w)];
    starts.extend(
        w.iter()
            .enumerate()
            .filter(|(_, word)| **word == "--")
            .map(|(i, _)| i + 1),
    );
    starts
        .into_iter()
        .find_map(|start| installer_at(&w, start))
        .unwrap_or_default()
}

/// The packages the installer at `i` names, or `None` when there is no installer there.
fn installer_at(w: &[&str], i: usize) -> Option<Vec<Requested>> {
    let word = *w.get(i)?;
    let rest = &w[i + 1..];
    let base = word.rsplit('/').next().unwrap_or(word);
    Some(match base {
        "npm" => match rest.first() {
            Some(&("install" | "i" | "add" | "in" | "isntall")) => npm_args(&rest[1..], false),
            Some(&("exec" | "x")) => npm_args(&rest[1..], true),
            _ => return None,
        },
        "pnpm" | "yarn" | "bun" => match rest.first() {
            Some(&("add" | "install" | "i")) => npm_args(&rest[1..], false),
            Some(&("dlx" | "x")) => npm_args(&rest[1..], true),
            _ => return None,
        },
        "npx" | "bunx" => npm_args(rest, true),
        "pip" | "pip3" => match rest.first() {
            Some(&"install") => pip_args(&rest[1..], false),
            _ => return None,
        },
        "python" | "python3" if rest.starts_with(&["-m", "pip", "install"]) => {
            pip_args(&rest[3..], false)
        }
        "uv" => match rest {
            ["pip", "install", args @ ..] => pip_args(args, false),
            ["add", args @ ..] => pip_args(args, false),
            ["tool", "install", args @ ..] => pip_args(args, true),
            _ => return None,
        },
        "poetry" if rest.first() == Some(&"add") => pip_args(&rest[1..], false),
        "pipx" if rest.first() == Some(&"install") => pip_args(&rest[1..], true),
        "uvx" => pip_args(rest, true),
        "cargo" => match rest.first() {
            Some(&("add" | "install")) => cargo_args(&rest[1..]),
            _ => return None,
        },
        _ => return None,
    })
}

/// npm, pnpm, yarn and bun options that take a value as the next word.
const NPM_VALUE_FLAGS: &[&str] = &[
    "-w",
    "--workspace",
    "--prefix",
    "-C",
    "--dir",
    "--cwd",
    "--cache",
    "--cache-folder",
    "--tag",
    "--omit",
    "--include",
    "--userconfig",
    "--globalconfig",
    "--install-strategy",
    "--before",
    "--loglevel",
    "--filter",
    "-F",
    "--backend",
    "--network-concurrency",
    "--modules-folder",
    "--mutex",
    "--save-prefix",
    "--lockfile-dir",
    "--virtual-store-dir",
    "--store-dir",
    "--reporter",
    "--config",
    "--shell",
    "--script-shell",
];

/// npm-style arguments: package specs, or for `npx` the one package it runs.
fn npm_args(args: &[&str], runs_one: bool) -> Vec<Requested> {
    let package = |spec: &str| {
        npm_name(spec).map(|name| Requested {
            ecosystem: Ecosystem::Npm,
            name,
        })
    };
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(arg) = args.get(i).copied() {
        i += 1;
        if arg.contains("registry") {
            return Vec::new();
        }
        if runs_one && (arg == "-p" || arg == "--package") {
            // The package to fetch; what follows is the command it provides.
            out.extend(args.get(i).copied().and_then(package));
            return out;
        }
        if runs_one {
            if let Some(spec) = arg.strip_prefix("--package=") {
                out.extend(package(spec));
                return out;
            }
            if arg == "-c" || arg == "--call" {
                return out;
            }
        }
        if NPM_VALUE_FLAGS.contains(&arg) {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        out.extend(package(arg));
        if runs_one {
            break;
        }
    }
    out
}

/// A package name from an npm spec, or `None` for a path, URL, git, GitHub or tarball spec.
pub(crate) fn npm_name(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty()
        || spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with('~')
        || spec.contains("://")
        || spec.starts_with("git")
        || spec.starts_with("file:")
        || spec.starts_with("github:")
        || spec.starts_with("workspace:")
        || spec.starts_with("link:")
        || spec.ends_with(".tgz")
        || spec.ends_with(".tar.gz")
    {
        return None;
    }
    // alias@npm:real@1
    let spec = match spec.split_once("@npm:") {
        Some((_, real)) => real,
        None => spec,
    };
    let name = if let Some(scoped) = spec.strip_prefix('@') {
        let scope_and_name = scoped.split('@').next().unwrap_or(scoped);
        if scope_and_name.matches('/').count() != 1 {
            return None;
        }
        format!("@{scope_and_name}")
    } else {
        let name = spec.split('@').next().unwrap_or(spec);
        // `user/repo` is a GitHub shorthand, not a package.
        if name.contains('/') {
            return None;
        }
        name.to_string()
    };
    let valid = name.len() <= 214
        && name.chars().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, '-' | '_' | '.' | '@' | '/' | '~')
        });
    (valid && !name.is_empty()).then_some(name)
}

/// Options that point pip, uv, pipx or poetry at another source: the command names nothing
/// on PyPI the bouncer can judge.
const PIP_SOURCE_FLAGS: &[&str] = &[
    "-i",
    "--index-url",
    "--extra-index-url",
    "--index",
    "--default-index",
    "--source",
    "-f",
    "--find-links",
    "--no-index",
];

/// pip, uv, pipx and poetry options that take a value as the next word.
const PIP_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--requirement",
    "-c",
    "--constraint",
    "--constraints",
    "-e",
    "--editable",
    "-t",
    "--target",
    "--platform",
    "--python-version",
    "--implementation",
    "--abi",
    "--root",
    "--prefix",
    "--src",
    "--upgrade-strategy",
    "--progress-bar",
    "--root-user-action",
    "--report",
    "--log",
    "--proxy",
    "--retries",
    "--timeout",
    "--exists-action",
    "--trusted-host",
    "--cert",
    "--client-cert",
    "--cache-dir",
    "--no-binary",
    "--only-binary",
    "--config-settings",
    "-C",
    "--global-option",
    "--group",
    "-G",
    "--optional",
    "--extra",
    "-E",
    "--extras",
    "--tag",
    "--branch",
    "--rev",
    "--marker",
    "-m",
    "--markers",
    "--python",
    "-p",
    "--directory",
    "--project",
    "--package",
    "--script",
    "--bounds",
    "--with",
    "--python-platform",
    "--overrides",
    "--build-constraints",
    "--config-file",
    "--resolution",
    "--prerelease",
    "--exclude-newer",
    "--link-mode",
    "--pip-args",
    "--suffix",
];

/// pip-style arguments. A requirements file, an editable path or another index names none.
fn pip_args(args: &[&str], runs_one: bool) -> Vec<Requested> {
    let package = |spec: &str| {
        pip_name(spec).map(|name| Requested {
            ecosystem: Ecosystem::PyPi,
            name,
        })
    };
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(arg) = args.get(i).copied() {
        i += 1;
        let flag = arg.split('=').next().unwrap_or(arg);
        if PIP_SOURCE_FLAGS.contains(&flag) {
            return Vec::new();
        }
        if runs_one && flag == "--from" {
            // `uvx --from pkg cmd`: the package, then the command it provides.
            let spec = arg.strip_prefix("--from=").or_else(|| args.get(i).copied());
            out.extend(spec.and_then(package));
            return out;
        }
        if PIP_VALUE_FLAGS.contains(&arg) {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        out.extend(package(arg));
        if runs_one {
            break;
        }
    }
    out
}

/// A normalized (PEP 503) project name from a requirement, or `None` for a path, URL or file.
pub(crate) fn pip_name(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty()
        || spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with('~')
        || spec.contains("://")
        || spec.contains(" @ ")
        || spec.ends_with(".whl")
        || spec.ends_with(".tar.gz")
        || spec.ends_with(".zip")
    {
        return None;
    }
    let end = spec
        .find(['=', '<', '>', '!', '~', '[', ';', '@', ' ', ','])
        .unwrap_or(spec.len());
    let raw = &spec[..end];
    if raw.is_empty()
        || !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    let mut name = String::new();
    let mut last_sep = false;
    for c in raw.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_sep {
                name.push('-');
            }
            last_sep = true;
        } else {
            name.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    Some(name)
}

fn cargo_args(args: &[&str]) -> Vec<Requested> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(*arg, "--git" | "--path" | "--registry" | "--index")
            || arg.starts_with("--git=")
            || arg.starts_with("--path=")
            || arg.starts_with("--registry=")
            || arg.starts_with("--index=")
        {
            return Vec::new();
        }
        if matches!(
            *arg,
            "--features"
                | "-F"
                | "--rename"
                | "--package"
                | "-p"
                | "--version"
                | "--vers"
                | "--branch"
                | "--tag"
                | "--rev"
                | "--root"
                | "--target"
                | "--bin"
                | "--manifest-path"
        ) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let name = arg.split('@').next().unwrap_or(arg);
        if !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            out.push(Requested {
                ecosystem: Ecosystem::Crates,
                name: name.to_ascii_lowercase(),
            });
        }
    }
    out
}

// -- Private registries ---------------------------------------------------------------------

fn ancestors_and_home(cwd: Option<&Path>, env: &Environment) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = cwd
        .map(|cwd| cwd.ancestors().map(Path::to_path_buf).collect())
        .unwrap_or_default();
    dirs.extend(env.home.clone());
    dirs
}

/// npm installs from somewhere other than the public registry here: the environment or an
/// `.npmrc` (the project's, a parent's or the user's) names another registry or a scoped one.
fn npm_registry_is_private(cwd: Option<&Path>, env: &Environment) -> bool {
    if ["NPM_CONFIG_REGISTRY", "npm_config_registry"]
        .iter()
        .any(|var| env.var(var).is_some_and(|url| !is_public_npm(url)))
    {
        return true;
    }
    ancestors_and_home(cwd, env).iter().any(|dir| {
        std::fs::read_to_string(dir.join(".npmrc")).is_ok_and(|rc| {
            rc.lines().any(|line| {
                let line = line.trim();
                !line.starts_with('#')
                    && !line.starts_with(';')
                    && line.split_once('=').is_some_and(|(key, url)| {
                        key.trim().ends_with("registry") && !is_public_npm(url)
                    })
            })
        })
    })
}

fn is_public_npm(url: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    url.is_empty() || url == "https://registry.npmjs.org" || url == "http://registry.npmjs.org"
}

/// The environment, the project or the user points Python installs at another index:
/// pip's or uv's variables, pip.conf, or uv's or poetry's sources.
fn python_index_is_private(cwd: Option<&Path>, env: &Environment) -> bool {
    if [
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
        "PIP_FIND_LINKS",
        "UV_INDEX_URL",
        "UV_EXTRA_INDEX_URL",
        "UV_INDEX",
        "UV_DEFAULT_INDEX",
    ]
    .iter()
    .any(|var| env.var(var).is_some())
    {
        return true;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in cwd
        .map(|cwd| cwd.ancestors().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        files.push(dir.join("pyproject.toml"));
        files.push(dir.join("uv.toml"));
        files.push(dir.join("pip.conf"));
    }
    if let Some(home) = &env.home {
        files.push(home.join(".pip/pip.conf"));
        files.push(home.join(".config/pip/pip.conf"));
        files.push(home.join(".config/uv/uv.toml"));
    }
    files.extend(env.system_pip_conf.clone());
    files.iter().any(|file| {
        std::fs::read_to_string(file).is_ok_and(|text| {
            text.contains("index-url")
                || text.contains("find-links")
                || text.contains("[[tool.uv.index]]")
                || text.contains("[[index]]")
                || text.contains("[[tool.poetry.source]]")
        })
    })
}

// -- Lookalikes -----------------------------------------------------------------------------

const POPULAR_NPM: &[&str] = &[
    "react",
    "react-dom",
    "next",
    "vue",
    "angular",
    "svelte",
    "express",
    "lodash",
    "axios",
    "typescript",
    "webpack",
    "vite",
    "eslint",
    "prettier",
    "jest",
    "vitest",
    "mocha",
    "chalk",
    "commander",
    "yargs",
    "dotenv",
    "moment",
    "dayjs",
    "uuid",
    "zod",
    "yup",
    "joi",
    "redux",
    "mongoose",
    "sequelize",
    "prisma",
    "knex",
    "pg",
    "mysql2",
    "redis",
    "ioredis",
    "socket.io",
    "cors",
    "body-parser",
    "jsonwebtoken",
    "bcrypt",
    "bcryptjs",
    "nodemon",
    "ts-node",
    "tsx",
    "rimraf",
    "glob",
    "minimist",
    "debug",
    "request",
    "node-fetch",
    "cross-env",
    "concurrently",
    "tailwindcss",
    "postcss",
    "autoprefixer",
    "sass",
    "babel-loader",
    "esbuild",
    "rollup",
    "puppeteer",
    "playwright",
    "cheerio",
    "jquery",
    "underscore",
    "ramda",
    "rxjs",
    "graphql",
    "apollo-server",
    "fastify",
    "koa",
    "hapi",
    "nestjs",
    "openai",
    "@anthropic-ai/sdk",
    "langchain",
    "stripe",
    "aws-sdk",
    "firebase",
    "supabase",
    "electron",
    "sharp",
    "multer",
    "nodemailer",
    "winston",
    "pino",
    "morgan",
    "helmet",
    "passport",
    "colors",
    "inquirer",
    "ora",
    "execa",
    "semver",
    "classnames",
    "clsx",
    "date-fns",
    "immer",
    "zustand",
    "swr",
];

const POPULAR_PYPI: &[&str] = &[
    "requests",
    "numpy",
    "pandas",
    "scipy",
    "matplotlib",
    "seaborn",
    "scikit-learn",
    "torch",
    "tensorflow",
    "keras",
    "flask",
    "django",
    "fastapi",
    "uvicorn",
    "gunicorn",
    "pydantic",
    "sqlalchemy",
    "psycopg2",
    "psycopg2-binary",
    "pymongo",
    "redis",
    "celery",
    "boto3",
    "botocore",
    "pytest",
    "black",
    "ruff",
    "mypy",
    "flake8",
    "pylint",
    "setuptools",
    "wheel",
    "pip",
    "virtualenv",
    "poetry",
    "httpx",
    "aiohttp",
    "beautifulsoup4",
    "lxml",
    "selenium",
    "playwright",
    "openai",
    "anthropic",
    "langchain",
    "transformers",
    "huggingface-hub",
    "tqdm",
    "click",
    "typer",
    "rich",
    "pyyaml",
    "python-dotenv",
    "jinja2",
    "pillow",
    "opencv-python",
    "cryptography",
    "paramiko",
    "urllib3",
    "certifi",
    "six",
    "attrs",
    "jsonschema",
    "pytz",
    "python-dateutil",
    "arrow",
    "loguru",
    "colorama",
    "tabulate",
    "openpyxl",
    "xlrd",
    "pyarrow",
    "polars",
    "dask",
    "networkx",
    "sympy",
    "statsmodels",
    "xgboost",
    "lightgbm",
    "catboost",
    "nltk",
    "spacy",
    "gensim",
    "streamlit",
    "gradio",
    "jupyter",
    "ipython",
    "notebook",
    "docker",
    "kubernetes",
    "pyjwt",
    "bcrypt",
    "passlib",
];

const POPULAR_CRATES: &[&str] = &[
    "serde",
    "serde_json",
    "tokio",
    "reqwest",
    "clap",
    "anyhow",
    "thiserror",
    "rand",
    "regex",
    "chrono",
    "log",
    "env_logger",
    "tracing",
    "tracing-subscriber",
    "futures",
    "async-trait",
    "hyper",
    "axum",
    "actix-web",
    "warp",
    "rocket",
    "sqlx",
    "diesel",
    "uuid",
    "lazy_static",
    "once_cell",
    "itertools",
    "rayon",
    "crossbeam",
    "parking_lot",
    "bytes",
    "base64",
    "sha2",
    "hex",
    "url",
    "toml",
    "serde_yaml",
    "image",
    "tempfile",
    "walkdir",
    "glob",
    "dirs",
];

fn popular(ecosystem: Ecosystem) -> &'static [&'static str] {
    match ecosystem {
        Ecosystem::Npm => POPULAR_NPM,
        Ecosystem::PyPi => POPULAR_PYPI,
        Ecosystem::Crates => POPULAR_CRATES,
    }
}

/// The popular package a name imitates: the same once `-`, `_` and `.` are dropped, or one
/// edit away (a letter added, dropped, changed or two swapped) for a name of five or more.
pub(crate) fn lookalike_of(package: &Requested) -> Option<&'static str> {
    let names = popular(package.ecosystem);
    if names.contains(&package.name.as_str()) {
        return None;
    }
    let squash = |s: &str| -> String {
        s.chars()
            .filter(|c| !matches!(c, '-' | '_' | '.'))
            .collect()
    };
    let name = squash(&package.name);
    names.iter().copied().find(|popular| {
        let other = squash(popular);
        name == other || (name.chars().count() >= 5 && one_edit_apart(&name, &other))
    })
}

fn one_edit_apart(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a == b {
        return false;
    }
    match a.len().abs_diff(b.len()) {
        0 => {
            let diffs: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
            diffs.len() == 1
                || (diffs.len() == 2
                    && diffs[1] == diffs[0] + 1
                    && a[diffs[0]] == b[diffs[1]]
                    && a[diffs[1]] == b[diffs[0]])
        }
        1 => {
            let (short, long) = if a.len() < b.len() {
                (&a, &b)
            } else {
                (&b, &a)
            };
            let i = short
                .iter()
                .zip(long.iter())
                .take_while(|(x, y)| x == y)
                .count();
            short[i..] == long[i + 1..]
        }
        _ => false,
    }
}

// -- The registries -------------------------------------------------------------------------

pub(crate) struct HttpRegistry {
    client: reqwest::Client,
}

impl HttpRegistry {
    pub(crate) fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(3_000))
            .user_agent("contextstream-package-bouncer (https://contextstream.io)")
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// The status of a GET, without reading the body.
    async fn status(&self, url: &str, accept: Option<&str>) -> Option<u16> {
        let mut request = self.client.get(url);
        if let Some(accept) = accept {
            request = request.header(reqwest::header::ACCEPT, accept);
        }
        request.send().await.ok().map(|r| r.status().as_u16())
    }

    async fn json(&self, url: &str) -> Option<Value> {
        let response = self.client.get(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<Value>().await.ok()
    }
}

fn exists(status: Option<u16>) -> Option<bool> {
    match status {
        Some(404) => Some(false),
        Some(code) if (200..300).contains(&code) => Some(true),
        _ => None,
    }
}

#[async_trait]
impl Registry for HttpRegistry {
    async fn lookup(&self, package: &Requested) -> Lookup {
        match package.ecosystem {
            Ecosystem::Npm => {
                let encoded = package.name.replace('/', "%2F");
                let status = self
                    .status(
                        &format!("https://registry.npmjs.org/{encoded}"),
                        Some("application/vnd.npm.install-v1+json"),
                    )
                    .await;
                match exists(status) {
                    Some(false) => Lookup::Missing,
                    None => Lookup::Unknown,
                    Some(true) => {
                        let downloads = self
                            .json(&format!(
                                "https://api.npmjs.org/downloads/point/last-week/{}",
                                package.name
                            ))
                            .await
                            .and_then(|v| v.get("downloads").and_then(Value::as_u64));
                        Lookup::Found {
                            weekly_downloads: downloads,
                        }
                    }
                }
            }
            Ecosystem::PyPi => {
                let status = self
                    .status(
                        &format!("https://pypi.org/pypi/{}/json", package.name),
                        None,
                    )
                    .await;
                match exists(status) {
                    Some(false) => Lookup::Missing,
                    None => Lookup::Unknown,
                    Some(true) => {
                        let downloads = self
                            .json(&format!(
                                "https://pypistats.org/api/packages/{}/recent",
                                package.name
                            ))
                            .await
                            .and_then(|v| v.pointer("/data/last_week").and_then(Value::as_u64));
                        Lookup::Found {
                            weekly_downloads: downloads,
                        }
                    }
                }
            }
            Ecosystem::Crates => {
                let url = format!("https://crates.io/api/v1/crates/{}", package.name);
                match self.json(&url).await {
                    Some(body) => Lookup::Found {
                        // crates.io counts the last 90 days; a thirteenth is about a week.
                        weekly_downloads: body
                            .pointer("/crate/recent_downloads")
                            .and_then(Value::as_u64)
                            .map(|d| d / 13),
                    },
                    None => match exists(self.status(&url, None).await) {
                        Some(false) => Lookup::Missing,
                        _ => Lookup::Unknown,
                    },
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "package_bouncer_tests.rs"]
mod tests;
