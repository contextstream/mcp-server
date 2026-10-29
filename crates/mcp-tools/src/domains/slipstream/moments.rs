//! Moments in a coding session where a ContextAgent can take work off the main agent, read
//! from [`moments.json`](moments.json) (ContextCode mirrors the same file, so both clients
//! pick the same agent for the same request).
//!
//! A moment fits a request (its prompt cues), a shell command about to run or that failed,
//! or a file edit. The agent that takes a moment is found from each agent's name and
//! one-line job, never from its id: a new agent whose job line says what it does fits
//! without a code change.

use serde::Deserialize;
use std::sync::OnceLock;

use super::auto::AutoAgent;

/// The moments table.
#[derive(Debug, Deserialize)]
pub struct MomentTable {
    pub version: u32,
    pub moments: Vec<Moment>,
    /// Words that ask for a change: a request with one is not a pure question.
    pub change_verbs: Vec<String>,
    /// Words a question starts with.
    pub question_words: Vec<String>,
    /// Words that negate the cue right after them ("don't upgrade the deps").
    #[serde(default)]
    pub negators: Vec<String>,
}

/// One moment.
#[derive(Debug, Deserialize)]
pub struct Moment {
    pub id: String,
    /// What the moment is, for the hint: "a dependency upgrade".
    pub why: String,
    #[serde(default)]
    pub prompt: PromptCues,
    /// Shell commands that are the moment when they are about to run.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Shell commands that are the moment when they fail.
    #[serde(default)]
    pub failed_commands: Vec<String>,
    /// Files whose edit is the moment.
    #[serde(default)]
    pub paths: Vec<PathCue>,
    /// What an agent's name and job say when it takes the moment.
    pub job: JobCues,
}

#[derive(Debug, Deserialize, Default)]
pub struct PromptCues {
    /// Alternatives; each is a list of word groups that must all have a word in the prompt.
    #[serde(default)]
    pub any_of: Vec<Vec<Vec<String>>>,
    /// Phrases any one of which fits.
    #[serde(default)]
    pub phrases: Vec<String>,
    /// Phrases that rule the moment out.
    #[serde(default)]
    pub not: Vec<String>,
    /// `question` (the prompt asks something), `no_change` (it asks for no change).
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PathCue {
    /// The file's name; a trailing `*` matches a prefix. Case does not matter.
    pub name: Option<String>,
    /// A folder anywhere in the path, with `ext` the extensions that count.
    pub dir: Option<String>,
    #[serde(default)]
    pub ext: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct JobCues {
    #[serde(default)]
    pub any_of: Vec<Vec<Vec<String>>>,
    /// Words that rule an agent out of the moment.
    #[serde(default)]
    pub not: Vec<String>,
}

/// The table, parsed once.
pub fn table() -> &'static MomentTable {
    static TABLE: OnceLock<MomentTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("moments.json")).expect("moments.json is valid")
    })
}

// ---------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------

/// A prompt's words, lowercase. `.`, `#` and `/` stay inside a word, so `v1.2`, `#42`,
/// `package.json` and URLs stay whole; a word's trailing punctuation goes.
pub(crate) fn prompt_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '#' | '/' | ':' | '_')))
        .map(|word| word.trim_matches(|c: char| matches!(c, '.' | '/' | ':')))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// An agent's name or job as words: lowercase, split at anything not a letter or digit.
pub(crate) fn job_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_version(word: &str) -> bool {
    let (had_v, rest) = match word.strip_prefix('v') {
        Some(rest) => (true, rest),
        None => (false, word),
    };
    let parts: Vec<&str> = rest.split('.').collect();
    !rest.is_empty()
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && (had_v || parts.len() > 1)
}

fn is_pr_ref(word: &str) -> bool {
    let number = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    word.strip_prefix('#').is_some_and(number)
        || word
            .split_once("/pull/")
            .is_some_and(|(_, rest)| number(rest.split('/').next().unwrap_or("")))
}

/// Whether one cue word matches one word.
fn cue_matches(cue: &str, word: &str) -> bool {
    match cue {
        "<version>" => is_version(word),
        "<pr-ref>" => is_pr_ref(word),
        _ => match cue.strip_suffix('*') {
            Some(prefix) => word.starts_with(prefix),
            None => word == cue,
        },
    }
}

fn group_hit(group: &[String], words: &[String]) -> bool {
    group
        .iter()
        .any(|cue| words.iter().any(|word| cue_matches(cue, word)))
}

/// `words` joined with single spaces and padded, for phrase matching.
fn joined(words: &[String]) -> String {
    format!(" {} ", words.join(" "))
}

fn has_phrase(haystack: &str, phrase: &str) -> bool {
    let phrase = prompt_words(phrase);
    !phrase.is_empty() && haystack.contains(&joined(&phrase))
}

// ---------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------

/// The moments a request fits, in table order.
pub fn prompt_moments(prompt: &str) -> Vec<&'static Moment> {
    let table = table();
    let words = prompt_words(prompt);
    if words.is_empty() {
        return Vec::new();
    }
    let text = joined(&words);
    table
        .moments
        .iter()
        .filter(|moment| {
            let cues = &moment.prompt;
            let fits =
                cues.any_of.iter().any(|groups| {
                    !groups.is_empty() && groups.iter().all(|g| group_hit(g, &words))
                }) || cues.phrases.iter().any(|phrase| has_phrase(&text, phrase));
            fits && !cues.not.iter().any(|phrase| has_phrase(&text, phrase))
                && !negated(cues, &words, &table.negators)
                && cues
                    .requires
                    .iter()
                    .all(|need| requirement_met(need, prompt, &words, table))
        })
        .collect()
}

/// Whether every hit of the moment's lead cues (the first word group of each `any_of`
/// entry, and each phrase's start) has a negator within the 3 words before it: the request
/// says not to do it ("don't upgrade the deps", "no need to update the packages").
fn negated(cues: &PromptCues, words: &[String], negators: &[String]) -> bool {
    let mut hits = Vec::new();
    for groups in &cues.any_of {
        if let Some(lead) = groups.first() {
            for (index, word) in words.iter().enumerate() {
                if lead.iter().any(|cue| cue_matches(cue, word)) {
                    hits.push(index);
                }
            }
        }
    }
    for phrase in &cues.phrases {
        let phrase = prompt_words(phrase);
        if phrase.is_empty() {
            continue;
        }
        for index in 0..words.len() {
            if words[index..].starts_with(&phrase) {
                hits.push(index);
            }
        }
    }
    !hits.is_empty()
        && hits.iter().all(|&index| {
            words[index.saturating_sub(3)..index]
                .iter()
                .any(|word| negators.iter().any(|negator| negator == word))
        })
}

fn requirement_met(need: &str, prompt: &str, words: &[String], table: &MomentTable) -> bool {
    match need {
        "question" => {
            prompt.contains('?')
                || words
                    .first()
                    .is_some_and(|first| table.question_words.iter().any(|q| q == first))
        }
        "no_change" => !words
            .iter()
            .any(|word| table.change_verbs.iter().any(|verb| verb == word)),
        _ => true,
    }
}

/// The shell command's parts, each as it starts: lowercase, single spaces, unquoted,
/// without leading `VAR=value`, `sudo`, `time`, `command`, `exec` or a `bash -lc` wrapper.
fn command_parts(command: &str) -> Vec<String> {
    command
        .split(['\n', ';', '|', '&'])
        .filter_map(|part| {
            let mut words: Vec<String> = part
                .split_whitespace()
                .map(|word| word.trim_matches(|c| c == '\'' || c == '"').to_lowercase())
                .filter(|word| !word.is_empty())
                .collect();
            while let Some(first) = words.first() {
                let is_assignment = first.split_once('=').is_some_and(|(name, _)| {
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
                let is_shell = matches!(first.as_str(), "bash" | "sh" | "zsh")
                    && words
                        .get(1)
                        .is_some_and(|flag| flag.starts_with('-') && flag.ends_with('c'));
                if is_shell {
                    words.drain(..2);
                } else if is_assignment
                    || matches!(first.as_str(), "sudo" | "time" | "command" | "exec")
                {
                    words.remove(0);
                } else {
                    break;
                }
            }
            (!words.is_empty()).then(|| words.join(" "))
        })
        .collect()
}

/// Whether a command part starts with `cue`: whole words, except that a cue ending in ` v`
/// takes a version right after it (`git tag v` matches `git tag v1.2`).
fn command_starts_with(part: &str, cue: &str) -> bool {
    let Some(rest) = part.strip_prefix(cue) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(next) if next.is_whitespace() => true,
        Some(next) => cue.ends_with(" v") && next.is_ascii_digit(),
    }
}

/// The moments a shell command is: about to run (`failed` false) or just failed.
pub fn command_moments(command: &str, failed: bool) -> Vec<&'static Moment> {
    let parts = command_parts(command);
    if parts.is_empty() {
        return Vec::new();
    }
    table()
        .moments
        .iter()
        .filter(|moment| {
            let cues = if failed {
                &moment.failed_commands
            } else {
                &moment.commands
            };
            cues.iter()
                .any(|cue| parts.iter().any(|part| command_starts_with(part, cue)))
        })
        .collect()
}

/// The moments an edit to `path` is.
pub fn path_moments(path: &str) -> Vec<&'static Moment> {
    let path = path.replace('\\', "/");
    let name = path.rsplit('/').next().unwrap_or(&path).to_lowercase();
    if name.is_empty() {
        return Vec::new();
    }
    let lowered = path.to_lowercase();
    table()
        .moments
        .iter()
        .filter(|moment| {
            moment.paths.iter().any(|cue| {
                if let Some(pattern) = &cue.name {
                    let pattern = pattern.to_lowercase();
                    return match pattern.strip_suffix('*') {
                        Some(prefix) => name.starts_with(prefix),
                        None => name == pattern,
                    };
                }
                if let Some(dir) = &cue.dir {
                    let dir = dir.to_lowercase();
                    let inside = lowered.starts_with(&format!("{dir}/"))
                        || lowered.contains(&format!("/{dir}/"));
                    let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
                    return inside
                        && (cue.ext.is_empty()
                            || cue.ext.iter().any(|e| e.eq_ignore_ascii_case(extension)));
                }
                false
            })
        })
        .collect()
}

/// The agent that takes `moment`: of the agents that may run automatically, the one whose
/// name and job match its job cues best (a word in the name counts twice). `None` when no
/// agent does that work.
pub fn agent_for<'a>(moment: &Moment, agents: &'a [AutoAgent]) -> Option<&'a AutoAgent> {
    let mut best: Option<(&AutoAgent, usize)> = None;
    for agent in agents.iter().filter(|agent| agent.may_run()) {
        let name = job_words(&agent.name);
        let job = job_words(&agent.job);
        let all: Vec<String> = name.iter().chain(job.iter()).cloned().collect();
        if moment
            .job
            .not
            .iter()
            .any(|cue| all.iter().any(|word| cue_matches(cue, word)))
        {
            continue;
        }
        let fits = moment
            .job
            .any_of
            .iter()
            .any(|groups| !groups.is_empty() && groups.iter().all(|g| group_hit(g, &all)));
        if !fits {
            continue;
        }
        let score: usize = moment
            .job
            .any_of
            .iter()
            .flatten()
            .flatten()
            .map(|cue| {
                2 * usize::from(name.iter().any(|w| cue_matches(cue, w)))
                    + usize::from(job.iter().any(|w| cue_matches(cue, w)))
            })
            .sum();
        let better = match best {
            None => true,
            Some((current, current_score)) => {
                score > current_score || (score == current_score && agent.id < current.id)
            }
        };
        if better {
            best = Some((agent, score));
        }
    }
    best.map(|(agent, _)| agent)
}

/// A pull request number the prompt names: `#42`, `PR 42`, `pull request 42`, `.../pull/42`.
pub fn pull_number(prompt: &str) -> Option<i64> {
    let words = prompt_words(prompt);
    for (index, word) in words.iter().enumerate() {
        if let Some(number) = word.strip_prefix('#').and_then(|n| n.parse().ok()) {
            return Some(number);
        }
        if let Some((_, rest)) = word.split_once("/pull/") {
            if let Ok(number) = rest.split('/').next().unwrap_or("").parse() {
                return Some(number);
            }
        }
        let after_pr = matches!(word.as_str(), "pr" | "request" | "pull");
        if after_pr {
            if let Some(number) = words.get(index + 1).and_then(|n| n.parse().ok()) {
                return Some(number);
            }
        }
    }
    None
}
