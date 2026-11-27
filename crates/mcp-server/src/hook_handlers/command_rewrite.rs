//! The editor hook's side of `contextstream-mcp run`: a shell command that a
//! tool the account has says it takes is handed to `run`, which sends it to the
//! tool and, where the tool does not take it, runs it right here.
//!
//! Only a Claude Code `Bash` call is changed, only when the hints the server
//! keeps (`mcp_client::command_hints`) say a tool takes the command as typed,
//! and only the command: the hook answers with the call's own input, its
//! `command` replaced by
//! `'<this binary>' run --tool <name> -- '<the command as typed>'`.
//! The hook makes no network call and gives no permission decision, so what
//! the editor would have asked about the original command it asks about the
//! changed one, which it shows.

use mcp_client::command_hints::{self, shell_quote};
use serde_json::{json, Value};

/// What the hook says to the editor about the model of the change.
const NOTE: &str = "This command was handed to `contextstream-mcp run`, which sends it to a tool your ContextStream account has for it when that tool takes it, and otherwise runs it here. Its output and exit status are the command's own; the last line says if it ran remotely.";

/// The hook's answer for a `Bash` call whose command a tool takes, or `None`
/// to leave the call as it is.
pub fn rewritten_bash(tool_input: &Value, cwd: &str) -> Option<Value> {
    if command_hints::off_in_env() {
        return None;
    }
    let _ = tool_input.get("command")?.as_str()?;
    let config = super::common::load_config(cwd);
    if !config.is_configured() {
        return None;
    }
    let cache = command_hints::read(
        &command_hints::local_identity(&config.api_key),
        &config.api_url,
    )?;
    let binary = std::env::current_exe().ok()?;
    rewrite_with_cache(tool_input, &binary.to_string_lossy(), &cache)
}

fn rewrite_with_cache(
    tool_input: &Value,
    binary: &str,
    cache: &command_hints::Cache,
) -> Option<Value> {
    let command = tool_input.get("command")?.as_str()?;
    let prefix = format!("{} run ", shell_quote(binary));
    // Already handed to `run`: never wrap a second time.
    if command.contains(&prefix) {
        return None;
    }
    // A line that names the switch as off runs as typed: the person or the agent said where.
    if command_hints::off_in_command(command) {
        return None;
    }
    if let Some(tool) = cache.tool_for(command) {
        return wrap(tool_input, binary, &tool.name, command);
    }
    let rewritten = rewrite_wrapped_command(command, &prefix, cache)?;
    Some(build_rewrite_output(tool_input, rewritten))
}

/// The hook's answer that replaces `command` in `tool_input` with `run` of it.
fn wrap(tool_input: &Value, binary: &str, tool: &str, command: &str) -> Option<Value> {
    let prefix = format!("{} run ", shell_quote(binary));
    // Already handed to `run`: not handed again.
    if command.trim_start().starts_with(&prefix) {
        return None;
    }
    let rewritten = format!(
        "{prefix}--tool {} -- {}",
        shell_quote(tool),
        shell_quote(command.trim())
    );
    Some(build_rewrite_output(tool_input, rewritten))
}

fn build_rewrite_output(tool_input: &Value, rewritten_command: String) -> Value {
    let mut updated = tool_input.clone();
    let original_cmd = tool_input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    updated["command"] = Value::String(rewritten_command);
    let tagged_desc = match updated
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(existing) if existing.contains("[MachineStream]") => existing.to_string(),
        Some(existing) => format!("{existing} [MachineStream]"),
        None => {
            let preview: String = original_cmd.chars().take(64).collect();
            if original_cmd.chars().count() > 64 {
                format!("[MachineStream] {preview}...")
            } else if preview.is_empty() {
                "[MachineStream] Remote check".to_string()
            } else {
                format!("[MachineStream] {preview}")
            }
        }
    };
    updated["description"] = Value::String(tagged_desc);
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
            "additionalContext": NOTE,
        }
    })
}

/// Words before a command that leave it a command: the shell runs what follows `if`, `then`, `do`, `!` or `time` as it
/// would alone, so the check after one is still the check.
const LEADING_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "do", "while", "until", "!", "time",
];
/// Words that start a list item this reader does not follow (a loop's variable and words, a function's name): the item
/// is left as it is, and the items around it are read as usual.
const OPAQUE_KEYWORDS: &[&str] = &["for", "select", "function", "coproc"];
/// A `case` ends its patterns with `)`, which this reader takes for the end of a group: the whole line is left alone.
const REFUSED_KEYWORDS: &[&str] = &["case", "esac"];
/// The longest line read. A line that edits a file from a here-document and then runs the check is long.
const MAX_LINE_LEN: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    ListSep,     // ; && || newline, and the ( ) { } of a group or subshell
    Background,  // & : what stands before it runs on its own and is left as typed
    Pipe,        // | |&
    OutputRedir, // > >> >| &> &>> (optionally with leading fd and/or attached target)
    DupRedir,    // >& [fd]>&[fd|-]
    InputRedir,  // < << <<< <& <>
}

#[derive(Debug, Clone, Copy)]
struct Token {
    start: usize,
    end: usize,
    kind: TokenKind,
    needs_target: bool,
}

fn token(start: usize, end: usize, kind: TokenKind) -> Token {
    Token {
        start,
        end,
        kind,
        needs_target: false,
    }
}

/// Tokenize a POSIX shell command line with byte spans.
///
/// What it does not follow it steps over whole, so the commands around it are still found: a command substitution or a
/// parameter expansion is part of its word, a here-document's body is skipped, a comment ends at its line, and the
/// parentheses and braces of a group or subshell separate the commands inside as `;` does. `None` for a line it cannot
/// read to the end (an unclosed quote or substitution, a here-document with no end, a `case`).
fn tokenize_shell(input: &str) -> Option<Vec<Token>> {
    if input.is_empty() || input.len() > MAX_LINE_LEN {
        return None;
    }
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut tokens = Vec::new();
    // The here-documents opened on the line being read: (the word that ends one, whether `<<-` strips its tabs).
    let mut pending: Vec<(Vec<u8>, bool)> = Vec::new();

    while i < len {
        // Skip horizontal whitespace, and a backslash at a line's end, which joins it to the next.
        while i < len && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }
        if bytes[i] == b'\\' && i + 1 < len && bytes[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        let b = bytes[i];
        match b {
            b'\n' => {
                tokens.push(token(i, i + 1, TokenKind::ListSep));
                i += 1;
                if !pending.is_empty() {
                    i = skip_here_documents(bytes, i, &mut pending)?;
                }
            }
            b';' => {
                if i + 1 < len && bytes[i + 1] == b';' {
                    return None; // case terminator
                }
                tokens.push(token(i, i + 1, TokenKind::ListSep));
                i += 1;
            }
            b'(' | b')' => {
                tokens.push(token(i, i + 1, TokenKind::ListSep));
                i += 1;
            }
            b'{' | b'}' if stands_alone(bytes, i) => {
                tokens.push(token(i, i + 1, TokenKind::ListSep));
                i += 1;
            }
            b'#' => {
                // A comment: to the end of its line.
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'&' => {
                if i + 1 < len && bytes[i + 1] == b'&' {
                    tokens.push(token(i, i + 2, TokenKind::ListSep));
                    i += 2;
                } else if i + 1 < len && bytes[i + 1] == b'>' {
                    let start = i;
                    i += 2;
                    if i < len && bytes[i] == b'>' {
                        i += 1;
                    }
                    let op_end = i;
                    i = scan_word_tail(bytes, i)?;
                    tokens.push(Token {
                        start,
                        end: i,
                        kind: TokenKind::OutputRedir,
                        needs_target: i == op_end,
                    });
                } else {
                    tokens.push(token(i, i + 1, TokenKind::Background));
                    i += 1;
                }
            }
            b'|' => {
                if i + 1 < len && bytes[i + 1] == b'|' {
                    tokens.push(token(i, i + 2, TokenKind::ListSep));
                    i += 2;
                } else if i + 1 < len && bytes[i + 1] == b'&' {
                    tokens.push(token(i, i + 2, TokenKind::Pipe));
                    i += 2;
                } else {
                    tokens.push(token(i, i + 1, TokenKind::Pipe));
                    i += 1;
                }
            }
            b'<' => {
                let (tok, next) = scan_input_redir(bytes, i, i, &mut pending)?;
                tokens.push(tok);
                i = next;
            }
            b'>' => {
                let (tok, next) = scan_output_redir(bytes, i, i)?;
                tokens.push(tok);
                i = next;
            }
            _ => {
                let start = i;
                // Check for fd-prefixed redirection like `2>&1`, `2>/dev/null`, `1>out`, `0<in`.
                let mut d = i;
                while d < len && bytes[d].is_ascii_digit() {
                    d += 1;
                }
                if d > i && d < len && bytes[d] == b'>' {
                    let (tok, next) = scan_output_redir(bytes, start, d)?;
                    tokens.push(tok);
                    i = next;
                } else if d > i && d < len && bytes[d] == b'<' {
                    let (tok, next) = scan_input_redir(bytes, start, d, &mut pending)?;
                    tokens.push(tok);
                    i = next;
                } else {
                    let next = scan_word_tail(bytes, i)?;
                    if next == i {
                        return None; // nothing this reader can step over
                    }
                    tokens.push(token(start, next, TokenKind::Word));
                    i = next;
                }
            }
        }
    }

    // A here-document whose body never came.
    if !pending.is_empty() {
        return None;
    }
    Some(tokens)
}

/// Whether the `{` or `}` at `i` is a group's own word and not part of another (`{a,b}`, `}x`): bash reads `{` as a
/// group only with a blank after it, and `}` only where a command could end.
fn stands_alone(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i + 1) {
        None => true,
        Some(next) if bytes[i] == b'{' => matches!(next, b' ' | b'\t' | b'\n'),
        Some(next) => matches!(next, b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b')'),
    }
}

/// An input redirection whose `<` is at `lt_pos` (after an optional fd from `start`). A here-document's end word is
/// kept in `pending`, so its body is skipped when its line ends.
fn scan_input_redir(
    bytes: &[u8],
    start: usize,
    lt_pos: usize,
    pending: &mut Vec<(Vec<u8>, bool)>,
) -> Option<(Token, usize)> {
    let len = bytes.len();
    let rest = &bytes[lt_pos..];
    if rest.starts_with(b"<<<") {
        // A here-string: its word is the redirection's.
        let mut i = lt_pos + 3;
        while i < len && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        let end = scan_word_tail(bytes, i)?;
        return Some((token(start, end, TokenKind::InputRedir), end));
    }
    if rest.starts_with(b"<<") {
        let mut i = lt_pos + 2;
        let strip_tabs = i < len && bytes[i] == b'-';
        if strip_tabs {
            i += 1;
        }
        while i < len && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        let word_start = i;
        let end = scan_word_tail(bytes, i)?;
        if end == word_start {
            return None;
        }
        // The word as the shell compares it: without its quotes and backslashes.
        let delimiter: Vec<u8> = bytes[word_start..end]
            .iter()
            .copied()
            .filter(|b| !matches!(b, b'\'' | b'"' | b'\\'))
            .collect();
        if delimiter.is_empty() {
            return None;
        }
        pending.push((delimiter, strip_tabs));
        return Some((token(start, end, TokenKind::InputRedir), end));
    }
    let mut i = lt_pos + 1;
    if i < len && matches!(bytes[i], b'>' | b'&') {
        i += 1;
    }
    let end = scan_word_tail(bytes, i)?;
    Some((token(start, end, TokenKind::InputRedir), end))
}

/// Step over the bodies of the here-documents opened on the line that ended just before `i`: each runs to the line
/// that is its end word alone. `None` when one never ends.
fn skip_here_documents(
    bytes: &[u8],
    mut i: usize,
    pending: &mut Vec<(Vec<u8>, bool)>,
) -> Option<usize> {
    for (delimiter, strip_tabs) in pending.drain(..) {
        loop {
            if i >= bytes.len() {
                return None;
            }
            let end = bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |at| i + at);
            let mut line = &bytes[i..end];
            if strip_tabs {
                while line.first() == Some(&b'\t') {
                    line = &line[1..];
                }
            }
            i = (end + 1).min(bytes.len());
            if line == delimiter.as_slice() {
                break;
            }
            if end >= bytes.len() {
                return None;
            }
        }
    }
    Some(i)
}

fn scan_output_redir(bytes: &[u8], start: usize, gt_pos: usize) -> Option<(Token, usize)> {
    let len = bytes.len();
    let mut i = gt_pos + 1;
    let is_dup = if i < len && bytes[i] == b'&' {
        i += 1;
        true
    } else if i < len && matches!(bytes[i], b'>' | b'|') {
        i += 1;
        false
    } else {
        false
    };
    let op_end = i;
    i = scan_word_tail(bytes, i)?;
    let kind = if is_dup {
        TokenKind::DupRedir
    } else {
        TokenKind::OutputRedir
    };
    Some((
        Token {
            start,
            end: i,
            kind,
            needs_target: i == op_end,
        },
        i,
    ))
}

/// The end of the word that starts at `i`: quotes, a command substitution (`$(…)`, backticks) and a parameter
/// expansion (`${…}`) are part of it, whatever they hold.
fn scan_word_tail(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    while i < len {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' => break,
            b'\'' => i = scan_single_quoted(bytes, i)?,
            b'"' => i = scan_double_quoted(bytes, i)?,
            b'`' => i = scan_backticks(bytes, i)?,
            b'\\' => {
                i += 1;
                if i >= len {
                    return None;
                }
                i += 1;
            }
            b'$' if bytes.get(i + 1) == Some(&b'(') => i = scan_parens(bytes, i + 1)?,
            b'$' if bytes.get(i + 1) == Some(&b'{') => i = scan_braces(bytes, i + 1)?,
            b'$' if bytes.get(i + 1) == Some(&b'\'') => {
                // $'…': a literal in which a backslash keeps the next character.
                i += 2;
                while i < len && bytes[i] != b'\'' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if i >= len {
                    return None;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    Some(i)
}

/// After the single-quoted text that opens at `i`.
fn scan_single_quoted(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < bytes.len() && bytes[i] != b'\'' {
        i += 1;
    }
    (i < bytes.len()).then_some(i + 1)
}

/// After the double-quoted text that opens at `i`; a substitution inside it is stepped over whole.
fn scan_double_quoted(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    i += 1;
    while i < len && bytes[i] != b'"' {
        match bytes[i] {
            b'\\' => i += 2,
            b'`' => i = scan_backticks(bytes, i)?,
            b'$' if bytes.get(i + 1) == Some(&b'(') => i = scan_parens(bytes, i + 1)?,
            b'$' if bytes.get(i + 1) == Some(&b'{') => i = scan_braces(bytes, i + 1)?,
            _ => i += 1,
        }
    }
    (i < len).then_some(i + 1)
}

/// After the backticks that open at `i`.
fn scan_backticks(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    i += 1;
    while i < len && bytes[i] != b'`' {
        i += if bytes[i] == b'\\' { 2 } else { 1 };
    }
    (i < len).then_some(i + 1)
}

/// After the parenthesis that closes the one at `i` (a `$(…)` or `$((…))`). `None` for what cannot be paired by
/// counting: a `case` inside (`;;`), a here-document inside.
fn scan_parens(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    let mut depth = 0usize;
    while i < len {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'\'' => i = scan_single_quoted(bytes, i)?,
            b'"' => i = scan_double_quoted(bytes, i)?,
            b'`' => i = scan_backticks(bytes, i)?,
            b'\\' => i += 2,
            b';' if bytes.get(i + 1) == Some(&b';') => return None,
            b'<' if bytes.get(i + 1) == Some(&b'<') && bytes.get(i + 2) != Some(&b'<') => {
                return None
            }
            _ => i += 1,
        }
    }
    None
}

/// After the brace that closes the one at `i` (a `${…}`).
fn scan_braces(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    let mut depth = 0usize;
    while i < len {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'\'' => i = scan_single_quoted(bytes, i)?,
            b'"' => i = scan_double_quoted(bytes, i)?,
            b'`' => i = scan_backticks(bytes, i)?,
            b'\\' => i += 2,
            b'$' if bytes.get(i + 1) == Some(&b'(') => i = scan_parens(bytes, i + 1)?,
            _ => i += 1,
        }
    }
    None
}

/// Rewrite the check commands of a longer line (e.g. `cargo check ... 2>&1 | grep ... | head; echo ${PIPESTATUS[0]}`)
/// by wrapping only each check, with the pipes, redirections and other commands around it as they were typed.
///
/// A check is the leading simple command of a list item: what stands between `;`, `&&`, `||`, a newline or a group's
/// parentheses and braces, up to its first pipe. So a line that changes folder, sets a variable from `$(…)`, edits a
/// file from a here-document, times the check (`time`), tests it (`if … ; then`) or groups it (`( … )`) still has its
/// check sent. Left as typed: a command put in the background (`&`), one that reads its input from a redirection, what
/// is inside a substitution, and a line that names the switch that turns remote checks off.
fn rewrite_wrapped_command(
    command: &str,
    prefix: &str,
    cache: &command_hints::Cache,
) -> Option<String> {
    let tokens = tokenize_shell(command)?;
    if tokens.is_empty() {
        return None;
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0;

    while idx < tokens.len() {
        let item_start = idx;
        while idx < tokens.len()
            && !matches!(tokens[idx].kind, TokenKind::ListSep | TokenKind::Background)
        {
            idx += 1;
        }
        let mut item = &tokens[item_start..idx];
        let background = idx < tokens.len() && tokens[idx].kind == TokenKind::Background;
        // Advance past the separator for the next iteration.
        if idx < tokens.len() {
            idx += 1;
        }
        if background {
            continue;
        }
        // The words before the command that leave it a command, and the ones that start what is not followed.
        let mut opaque = false;
        while let Some(first) = item.first().filter(|first| first.kind == TokenKind::Word) {
            let word = &command[first.start..first.end];
            if REFUSED_KEYWORDS.contains(&word) {
                return None;
            }
            if OPAQUE_KEYWORDS.contains(&word) {
                opaque = true;
                break;
            }
            if !LEADING_KEYWORDS.contains(&word) {
                break;
            }
            item = &item[1..];
        }
        if opaque || item.is_empty() {
            continue;
        }
        // Inspect the leading simple command of this list item (before the first pipe/redirect).
        let mut word_end_idx = 0;
        while word_end_idx < item.len() && item[word_end_idx].kind == TokenKind::Word {
            word_end_idx += 1;
        }
        if word_end_idx == 0 {
            continue;
        }
        // Validate any redirections between the simple command and the first pipe / end of item.
        let mut j = word_end_idx;
        let mut valid_tail = true;
        while j < item.len() {
            match item[j].kind {
                TokenKind::Pipe => break,
                TokenKind::InputRedir => {
                    valid_tail = false;
                    break;
                }
                TokenKind::OutputRedir | TokenKind::DupRedir => {
                    if item[j].needs_target {
                        if j + 1 >= item.len() || item[j + 1].kind != TokenKind::Word {
                            valid_tail = false;
                            break;
                        }
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                TokenKind::Word | TokenKind::ListSep | TokenKind::Background => {
                    // Stray argument after a redirect (e.g. `cargo check > out --workspace`):
                    // never split arguments away from the command.
                    valid_tail = false;
                    break;
                }
            }
        }
        if !valid_tail {
            continue;
        }

        let cmd_start = item[0].start;
        let cmd_end = item[word_end_idx - 1].end;
        let candidate = command[cmd_start..cmd_end].trim();
        if candidate.is_empty() {
            continue;
        }
        if let Some(tool) = cache.tool_for(candidate) {
            replacements.push((
                cmd_start,
                cmd_end,
                format!(
                    "{prefix}--tool {} -- {}",
                    shell_quote(&tool.name),
                    shell_quote(candidate)
                ),
            ));
        }
    }

    if replacements.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(command.len() + replacements.len() * 64);
    let mut cursor = 0;
    for (start, end, replacement) in replacements {
        out.push_str(&command[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&command[cursor..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_replaced_and_the_rest_of_the_call_is_kept() {
        let input = json!({
            "command": "cargo test -p thing 2>&1 | tail -5",
            "description": "Run the tests",
            "timeout": 120000,
            "run_in_background": false,
        });
        let output = wrap(
            &input,
            "/usr/local/bin/contextstream-mcp",
            "check",
            "cargo test -p thing 2>&1 | tail -5",
        )
        .unwrap();
        let specific = &output["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        // No permission decision: the editor decides about the changed command as it would any.
        assert!(specific.get("permissionDecision").is_none());
        let updated = &specific["updatedInput"];
        assert_eq!(
            updated["command"],
            "'/usr/local/bin/contextstream-mcp' run --tool 'check' -- 'cargo test -p thing 2>&1 | tail -5'"
        );
        assert_eq!(updated["description"], "Run the tests [MachineStream]");
        assert_eq!(updated["timeout"], 120000);
        assert_eq!(updated["run_in_background"], false);
        assert!(specific["additionalContext"]
            .as_str()
            .unwrap()
            .contains("run"));

        // Missing description gets a compact `[MachineStream]` header label.
        let no_desc = wrap(
            &json!({"command": "cargo check --workspace"}),
            "/usr/local/bin/contextstream-mcp",
            "check",
            "cargo check --workspace",
        )
        .unwrap();
        assert_eq!(
            no_desc["hookSpecificOutput"]["updatedInput"]["description"],
            "[MachineStream] cargo check --workspace"
        );
    }

    #[test]
    fn quotes_in_the_command_cannot_end_its_quoting() {
        let nasty = "cargo test 'a b' \"c\" ; echo $HOME `id` \\ # x";
        let output = wrap(&json!({"command": nasty}), "/bin/b", "check", nasty).unwrap();
        let line = output["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_string();
        // What a POSIX shell would read back is the binary, then run, --tool, check, --, the command.
        assert_eq!(
            line,
            "'/bin/b' run --tool 'check' -- 'cargo test '\\''a b'\\'' \"c\" ; echo $HOME `id` \\ # x'"
        );
    }

    #[test]
    fn a_command_already_handed_to_run_is_left_alone() {
        let wrapped = "'/bin/b' run --tool 'check' -- 'cargo test'";
        assert!(wrap(&json!({"command": wrapped}), "/bin/b", "check", wrapped).is_none());
        assert!(wrap(
            &json!({"command": format!("  {wrapped}")}),
            "/bin/b",
            "check",
            &format!("  {wrapped}")
        )
        .is_none());
        // A different binary's `run` is not ours to skip.
        assert!(wrap(&json!({"command": wrapped}), "/other/b", "check", wrapped).is_some());
    }

    #[test]
    fn only_a_command_given_is_looked_at() {
        assert!(rewritten_bash(&json!({"description": "x"}), "/tmp").is_none());
        assert!(rewritten_bash(&json!({"command": 7}), "/tmp").is_none());
        assert!(rewritten_bash(&Value::Null, "/tmp").is_none());
    }

    fn test_cache() -> command_hints::Cache {
        command_hints::Cache::new(
            "id",
            "https://api.example",
            vec![command_hints::CachedTool {
                name: "check".to_string(),
                transport: "stream".to_string(),
                attachments: vec!["checkout_snapshot".to_string()],
                hints: command_hints::Hints {
                    argument: "command".to_string(),
                    cwd_argument: Some("cwd".to_string()),
                    patterns: vec![
                        r#"^(cd [A-Za-z0-9._/-]+ && )?([A-Z][A-Z0-9_]*=(?:[A-Za-z0-9._:@+,=/-]+|"[A-Za-z0-9._:@+,=/ -]+") )*(cargo (check|clippy|test)( [A-Za-z0-9=:,._/+-]+)*)( 2>&1)?( ?\| ?tail( -n ?[0-9]+| -[0-9]+)?)?$"#.to_string(),
                    ],
                },
            }],
        )
    }

    #[test]
    fn wrapped_check_commands_wrap_only_the_check_and_keep_pipes_and_suffixes() {
        let cache = test_cache();
        let bin = "/usr/local/bin/contextstream-mcp";

        let cmd = r#"cargo check --workspace --all-targets 2>&1 | grep -E "error|warning" | head -n 40; echo "exit: ${PIPESTATUS[0]}""#;
        let out = rewrite_with_cache(&json!({"command": cmd}), bin, &cache).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            format!(
                "'{bin}' run --tool 'check' -- 'cargo check --workspace --all-targets' 2>&1 | grep -E \"error|warning\" | head -n 40; echo \"exit: ${{PIPESTATUS[0]}}\""
            )
        );

        let env_cmd = r#"CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --all-features 2>&1 | grep error | head -20; echo "exit: ${PIPESTATUS[0]}""#;
        let out = rewrite_with_cache(&json!({"command": env_cmd}), bin, &cache).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            format!(
                "'{bin}' run --tool 'check' -- 'CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --all-features' 2>&1 | grep error | head -20; echo \"exit: ${{PIPESTATUS[0]}}\""
            )
        );

        let cd_cmd = "cd crates/ruff && cargo check --all-targets 2>&1 | head -n 50";
        let out = rewrite_with_cache(&json!({"command": cd_cmd}), bin, &cache).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            format!("cd crates/ruff && '{bin}' run --tool 'check' -- 'cargo check --all-targets' 2>&1 | head -n 50")
        );

        let redir_cmd = r#"cargo test -p ruff_linter > /tmp/out.txt 2>&1; echo "exit: $?""#;
        let out = rewrite_with_cache(&json!({"command": redir_cmd}), bin, &cache).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            format!("'{bin}' run --tool 'check' -- 'cargo test -p ruff_linter' > /tmp/out.txt 2>&1; echo \"exit: $?\"")
        );

        // Native tail match stays whole-command wrapped.
        let tail_cmd = "cargo check --workspace 2>&1 | tail -50";
        let out = rewrite_with_cache(&json!({"command": tail_cmd}), bin, &cache).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["command"],
            format!("'{bin}' run --tool 'check' -- 'cargo check --workspace 2>&1 | tail -50'")
        );
    }

    #[test]
    fn unsafe_or_non_check_wrapped_commands_are_left_alone() {
        let cache = test_cache();
        let bin = "/usr/local/bin/contextstream-mcp";
        for cmd in [
            "cargo test < input.txt",
            "cargo test <<EOF\nhi\nEOF",
            "cargo test <<< 'input'",
            "cargo check &",
            "cargo check > out.txt --workspace",
            "echo \"cargo check\" | grep cargo",
            "ls -la | head -n 20",
            "'/usr/local/bin/contextstream-mcp' run --tool 'check' -- 'cargo check' 2>&1 | head -n 20",
            // What a substitution holds is its word's, never a command of the line.
            "X=$(cargo check 2>&1 | tail -1); echo \"$X\"",
            "echo `cargo check`",
            "echo \"$(cargo test)\"",
            // A loop's own words, a case, and what this reader cannot read to its end.
            "for cargo in check; do echo $cargo; done",
            "case $1 in a) cargo check;; esac",
            "cargo check; echo 'unclosed",
            "cargo check; echo $(unclosed",
            "python3 - <<EOF\nprint(1)\ncargo check",
            // A comment and a here-document's body are text.
            "echo done # cargo check",
            "cat > notes.txt <<'EOF'\ncargo check\nEOF",
            "cat <<-END\n\tcargo test\n\tEND\necho ok",
        ] {
            assert!(
                rewrite_with_cache(&json!({"command": cmd}), bin, &cache).is_none(),
                "expected None for: {cmd}"
            );
        }
    }

    // What the lines agents type look like (35,020 of them, 2026-11-27): the check stands beside a `cd` to a worktree,
    // a variable set from `$(…)`, a file edited from a here-document, a group, `time` or `if`. Each check is sent.
    #[test]
    fn a_check_beside_what_this_reader_steps_over_is_still_sent() {
        let cache = test_cache();
        let bin = "/usr/local/bin/contextstream-mcp";
        let run = |command: &str| format!("'{bin}' run --tool 'check' -- '{command}'");
        let rewritten = |cmd: &str| {
            rewrite_with_cache(&json!({"command": cmd}), bin, &cache)
                .map(|out| {
                    out["hookSpecificOutput"]["updatedInput"]["command"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .unwrap_or_else(|| panic!("expected a rewrite of: {cmd}"))
        };
        for (cmd, expected) in [
            (
                "H=$(mktemp -d) && export HOME=$H && cargo check 2>&1 | tail -20; echo \"exit=${PIPESTATUS[0]}\"".to_string(),
                format!("H=$(mktemp -d) && export HOME=$H && {} 2>&1 | tail -20; echo \"exit=${{PIPESTATUS[0]}}\"", run("cargo check")),
            ),
            (
                "python3 - <<'EOF'\nimport re  # cargo check (text)\nprint(\"it's )\")\nEOF\ncargo test -p thing 2>&1 | tail -5".to_string(),
                format!("python3 - <<'EOF'\nimport re  # cargo check (text)\nprint(\"it's )\")\nEOF\n{} 2>&1 | tail -5", run("cargo test -p thing")),
            ),
            (
                "cat > a.txt <<EOF && cargo check\nline (one\nEOF\ncargo test".to_string(),
                format!("cat > a.txt <<EOF && {}\nline (one\nEOF\n{}", run("cargo check"), run("cargo test")),
            ),
            (
                "cd /home/me/wt && (cargo check 2>&1 | grep -E 'error|warning'; echo \"--- tests\"; cargo test -p a 2>&1 | tail -3)".to_string(),
                format!("cd /home/me/wt && ({} 2>&1 | grep -E 'error|warning'; echo \"--- tests\"; {} 2>&1 | tail -3)", run("cargo check"), run("cargo test -p a")),
            ),
            (
                "{ cargo check; cargo test; } > out.log 2>&1".to_string(),
                format!("{{ {}; {}; }} > out.log 2>&1", run("cargo check"), run("cargo test")),
            ),
            (
                "time cargo check --workspace".to_string(),
                format!("time {}", run("cargo check --workspace")),
            ),
            (
                "if cargo check; then echo ok; else cargo test; fi".to_string(),
                format!("if {}; then echo ok; else {}; fi", run("cargo check"), run("cargo test")),
            ),
            (
                "! cargo check || echo broken".to_string(),
                format!("! {} || echo broken", run("cargo check")),
            ),
            (
                "for p in a b; do echo $p; done; cargo test".to_string(),
                format!("for p in a b; do echo $p; done; {}", run("cargo test")),
            ),
            (
                "sleep 30 & cargo check".to_string(),
                format!("sleep 30 & {}", run("cargo check")),
            ),
            (
                "echo start # then: cargo check\ncargo test".to_string(),
                format!("echo start # then: cargo check\n{}", run("cargo test")),
            ),
            (
                "echo \"at $(date +%s) in ${PWD:-.}\"; cargo check".to_string(),
                format!("echo \"at $(date +%s) in ${{PWD:-.}}\"; {}", run("cargo check")),
            ),
            (
                "diff <(cargo check 2>&1) expected.txt".to_string(),
                format!("diff <({} 2>&1) expected.txt", run("cargo check")),
            ),
            (
                "echo a{b,c} }x; cargo check".to_string(),
                format!("echo a{{b,c}} }}x; {}", run("cargo check")),
            ),
        ] {
            assert_eq!(rewritten(&cmd), expected, "{cmd}");
        }
        // A line longer than a pattern's reach is still read: its check is found after a long here-document.
        let long = format!(
            "cat > big.txt <<'EOF'\n{}\nEOF\ncargo check",
            "x".repeat(20_000)
        );
        assert!(rewritten(&long).ends_with(&run("cargo check")));
    }

    // The switch named on the line keeps the whole line here, under the name the docs give and the one agents write.
    #[test]
    fn a_line_that_switches_remote_checks_off_is_left_as_typed() {
        let cache = test_cache();
        let bin = "/usr/local/bin/contextstream-mcp";
        for cmd in [
            "CONTEXTSTREAM_HOSTED_TOOLS=off cargo check",
            "CONTEXTSTREAM_REMOTE_CHECKS=off cargo test -p thing 2>&1 | tail -5",
            "cd wt && export PATH=/usr/local/go/bin:$PATH CONTEXTSTREAM_REMOTE_CHECKS=off; cargo check; cargo test",
            "CONTEXTSTREAM_REMOTE_CHECKS='0' cargo check",
            "cargo check; CONTEXTSTREAM_HOSTED_TOOLS=false cargo test",
        ] {
            assert!(command_hints::off_in_command(cmd), "{cmd}");
            assert!(
                rewrite_with_cache(&json!({"command": cmd}), bin, &cache).is_none(),
                "expected None for: {cmd}"
            );
        }
        for cmd in [
            "CONTEXTSTREAM_REMOTE_CHECKS=on cargo check",
            "MY_CONTEXTSTREAM_REMOTE_CHECKS=off cargo check",
            "CONTEXTSTREAM_REMOTE_CHECKS=offline cargo check",
            "cargo check",
        ] {
            assert!(!command_hints::off_in_command(cmd), "{cmd}");
            assert!(
                rewrite_with_cache(&json!({"command": cmd}), bin, &cache).is_some(),
                "expected a rewrite of: {cmd}"
            );
        }
    }
}
