//! The environment a call is made from, described so the service can tell
//! whether it can reproduce it.
//!
//! It goes in the `hello` of a call that runs a command on a checkout
//! snapshot, and holds facts about the machine and nothing of the checkout: the
//! operating system and CPU architecture names, the CPU count, whether the
//! process runs as root, the time zone's name, and the first line of
//! `<tool> --version` for each tool the API's request names (`versions`: data
//! from the API, no list of tools lives here). Nothing else about the machine,
//! its variables or its files is read or sent. A tool's line is cleaned before
//! it is sent (see [`redact`]): words in it that look like paths are left out,
//! the user name, the home folder's name and the host name are replaced, and
//! what is left is cut to 200 characters.
//!
//! A tool is asked its version by running it directly (no shell) in the
//! command's folder, so a version manager that picks the version by folder
//! answers as the command would see it. The program that runs is found only in
//! the folders of the caller's own `PATH` that are absolute and outside the
//! repository, never in the repository or the command's folder, so a cloned
//! repository cannot supply it. The tool has two seconds, is told not to
//! install anything, and its answer is cut to one short line.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// `off`, `0`, `false`, `no` or `disabled` sends no `environment` at all, and
/// the service then treats it as unknown.
pub const ENV_SWITCH: &str = "CONTEXTSTREAM_HOSTED_ENV";

/// How many tools one call asks about.
const MAX_TOOLS: usize = 16;
/// How long one tool has to answer.
const PROBE_LIMIT: Duration = Duration::from_secs(2);
/// How much of a tool's answer is kept.
const MAX_LINE_CHARS: usize = 200;
/// What stands where a user, home folder or host name was.
const REDACTED: &str = "<redacted>";
/// Names shorter than this are not replaced: they would also match ordinary
/// words of a version line.
const MIN_NAME_CHARS: usize = 3;
/// How much of a tool's output is read at all.
const MAX_OUTPUT_BYTES: u64 = 4096;
/// The longest time zone name sent.
const MAX_ZONE_CHARS: usize = 64;

/// The environment to send for a call, or `None` when [`ENV_SWITCH`] turns it
/// off. `root` is the repository's top folder; the request's `cwd` is a folder
/// inside it.
pub async fn describe(request: &Value, root: &Path) -> Option<Value> {
    if crate::command_hints::switched_off(std::env::var(ENV_SWITCH).ok().as_deref()) {
        return None;
    }
    let path = std::env::var_os("PATH");
    let hidden = Names::current();
    Some(
        describe_using(
            request,
            root,
            |name| find_program(name, path.as_deref(), root),
            &hidden,
        )
        .await,
    )
}

/// [`describe`], with `program` choosing what runs for a tool's name (`None`:
/// there is no such program for it) and `hidden` the names a tool's line must
/// not carry.
async fn describe_using(
    request: &Value,
    root: &Path,
    program: impl Fn(&str) -> Option<PathBuf>,
    hidden: &Names,
) -> Value {
    let folder = probe_folder(
        root,
        request.get("cwd").and_then(Value::as_str).unwrap_or(""),
    );
    let names = tool_names(request);
    let (program, folder) = (&program, &folder);
    let answers = futures::future::join_all(names.iter().map(|name| async move {
        let program = program(name.as_str())?;
        version_of(program.as_os_str(), folder, PROBE_LIMIT, hidden).await
    }))
    .await;
    let versions: Map<String, Value> = names
        .into_iter()
        .zip(answers)
        .map(|(name, answer)| (name, answer.map_or(Value::Null, Value::String)))
        .collect();
    json!({
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "cpus": cpus(),
        "root": is_root(),
        "tz": time_zone(std::env::var("TZ").ok().as_deref(), localtime_target().as_deref()),
        "versions": versions,
    })
}

/// The tools the API's request asks about (`versions`): bare names of up to
/// 32 characters from `a-z 0-9 . _ -`, not starting with `.` or `-`, each once,
/// the first 16. Anything else is not asked about.
pub fn tool_names(request: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in request
        .get("versions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if is_tool_name(name) && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
            if names.len() == MAX_TOOLS {
                break;
            }
        }
    }
    names
}

fn is_tool_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with(['.', '-'])
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// The folder a tool is asked in: the command's own, when it is a folder inside
/// the repository; otherwise the repository's top folder.
pub fn probe_folder(root: &Path, cwd: &str) -> PathBuf {
    let relative = Path::new(cwd);
    let inside = !cwd.is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if inside {
        let folder = root.join(relative);
        if folder.is_dir() {
            return folder;
        }
    }
    root.to_path_buf()
}

/// The file a tool's name stands for: the first one of that name in a folder of
/// `path` (the caller's `PATH`, split the way the platform splits it) that is an
/// absolute path outside `repository`. A relative or empty entry (which would
/// mean the folder the command runs in) is skipped, and so is a file that
/// resolves, through links, into the repository. The program is then run by
/// that full path, so nothing is looked up again in the command's folder.
pub fn find_program(name: &str, path: Option<&OsStr>, repository: &Path) -> Option<PathBuf> {
    let repository = repository.canonicalize().ok()?;
    for dir in std::env::split_paths(path?) {
        if !dir.is_absolute() {
            continue;
        }
        for file in candidates(&dir, name) {
            let Ok(real) = file.canonicalize() else {
                continue;
            };
            if real.starts_with(&repository) || !is_program(&real) {
                continue;
            }
            return Some(file);
        }
    }
    None
}

/// The files `name` can be called in `dir`.
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        // Only what runs by itself: a script runs through a shell.
        ["exe", "com"]
            .iter()
            .map(|extension| dir.join(format!("{name}.{extension}")))
            .collect()
    } else {
        vec![dir.join(name)]
    }
}

#[cfg(unix)]
fn is_program(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_program(file: &Path) -> bool {
    file.is_file()
}

/// What `<program> --version` printed: its first non-empty line (standard
/// output, else standard error), cleaned by [`redact`]. `None` when it is not
/// there, fails, prints nothing, is nothing but paths or is not done in `limit`.
async fn version_of(
    program: &OsStr,
    folder: &Path,
    limit: Duration,
    hidden: &Names,
) -> Option<String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .current_dir(folder)
        // A probe must never install or ask about installing anything.
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let run = async {
        let mut child = command.spawn().ok()?;
        let out = read_some(child.stdout.take()?);
        let err = read_some(child.stderr.take()?);
        let (out, err, status) = tokio::join!(out, err, child.wait());
        if !status.ok()?.success() {
            return None;
        }
        let line = first_line(&out).or_else(|| first_line(&err))?;
        redact(&line, hidden)
    };
    tokio::time::timeout(limit, run).await.ok().flatten()
}

/// The first [`MAX_OUTPUT_BYTES`] of `pipe`. The rest is read and dropped, so a
/// tool that says more than that is not left waiting for someone to read it.
async fn read_some(mut pipe: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut pipe)
        .take(MAX_OUTPUT_BYTES)
        .read_to_end(&mut kept)
        .await;
    let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
    kept
}

/// The first line of `output` that is not blank, trimmed.
pub fn first_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// A tool's line as it may be sent: every word that looks like a path is left
/// out (it contains a `/` or a `\`, or starts with `~`; a drive path such as
/// `C:\tools` has a separator too), the user name, home folder name and host
/// name in `hidden` are replaced by `<redacted>` wherever they stand as whole
/// words, spaces are collapsed to one, and the rest is cut to 200 characters.
/// `None` when nothing is left.
pub fn redact(line: &str, hidden: &Names) -> Option<String> {
    let kept: Vec<&str> = line
        .split_whitespace()
        .filter(|word| !looks_like_path(word))
        .collect();
    let cleaned = hidden.hide_in(&kept.join(" "));
    (!cleaned.is_empty()).then(|| cleaned.chars().take(MAX_LINE_CHARS).collect())
}

/// Whether a word of a version line names a place on a disk or a server.
fn looks_like_path(word: &str) -> bool {
    word.contains(['/', '\\']) || word.trim_start_matches(OPENING).starts_with('~')
}

/// What may stand before a path in running text.
const OPENING: [char; 8] = ['(', '[', '{', '<', '"', '\'', '`', '='];

/// Names a tool's line must not carry: who runs it and on which machine.
#[derive(Debug, Default, Clone)]
pub struct Names {
    /// Longest first, so a whole host name goes before its first label does.
    words: Vec<String>,
}

impl Names {
    /// The user name (`USER`, `USERNAME` or `LOGNAME`), the last component of
    /// the home folder and the host name, with the host's first label.
    pub fn current() -> Self {
        let from_env = |name: &str| std::env::var(name).ok();
        let user = from_env("USER")
            .or_else(|| from_env("USERNAME"))
            .or_else(|| from_env("LOGNAME"));
        let home = dirs::home_dir();
        Self::from_parts(user.as_deref(), home.as_deref(), host_name().as_deref())
    }

    /// The names for a user, a home folder and a host. A name of fewer than
    /// [`MIN_NAME_CHARS`] characters is left out.
    pub fn from_parts(user: Option<&str>, home: Option<&Path>, host: Option<&str>) -> Self {
        let home_name = home.and_then(Path::file_name).and_then(OsStr::to_str);
        let host_label = host.and_then(|name| name.split('.').next());
        let mut words: Vec<String> = Vec::new();
        for candidate in [user, home_name, host, host_label].into_iter().flatten() {
            let candidate = candidate.trim();
            if candidate.chars().count() >= MIN_NAME_CHARS
                && !words
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(candidate))
            {
                words.push(candidate.to_string());
            }
        }
        words.sort_by_key(|word| std::cmp::Reverse(word.len()));
        Self { words }
    }

    /// `text` with each of the names replaced by `<redacted>` where it stands
    /// as a whole word, whatever its letter case.
    pub fn hide_in(&self, text: &str) -> String {
        self.words
            .iter()
            .fold(text.to_string(), |text, word| replace_word(&text, word))
    }
}

/// `text` with every whole-word, case-insensitive (ASCII) occurrence of `word`
/// replaced by `<redacted>`. A word is whole when no letter, digit or `_`
/// stands next to it.
fn replace_word(text: &str, word: &str) -> String {
    if word.is_empty() {
        return text.to_string();
    }
    // ASCII lowering keeps every byte offset and character boundary.
    let lower_text = text.to_ascii_lowercase();
    let lower_word = word.to_ascii_lowercase();
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower_text[at..].find(&lower_word) {
        let start = at + found;
        let end = start + lower_word.len();
        out.push_str(&text[at..start]);
        let whole = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c))
            && text[end..].chars().next().is_none_or(|c| !is_word_char(c));
        out.push_str(if whole { REDACTED } else { &text[start..end] });
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(unix)]
fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and gethostname writes
    // at most that many bytes into it.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec()).ok()
}

#[cfg(not(unix))]
fn host_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |count| count.get())
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// Where `/etc/localtime` points, on a system that keeps it as a link.
fn localtime_target() -> Option<PathBuf> {
    std::fs::read_link("/etc/localtime").ok()
}

/// The zone in effect: `TZ` when it is set (a leading `:` is not part of the
/// name), else the zone name in the target of `/etc/localtime`; empty when
/// neither says.
pub fn time_zone(tz: Option<&str>, localtime: Option<&Path>) -> String {
    let named = tz
        .map(|zone| zone.trim().trim_start_matches(':'))
        .filter(|zone| !zone.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let target = localtime?.to_string_lossy().into_owned();
            let (_, zone) = target.rsplit_once("zoneinfo/")?;
            Some(zone.to_string()).filter(|zone| !zone.is_empty())
        });
    named
        .map(|zone| zone.chars().take(MAX_ZONE_CHARS).collect())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
