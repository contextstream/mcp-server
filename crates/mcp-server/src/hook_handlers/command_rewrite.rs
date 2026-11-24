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
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
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

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "coproc", "!",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    ListSep,     // ; && || \n
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

/// Tokenize a top-level POSIX shell command line with byte spans, refusing subshells,
/// command groups, heredocs, background `&`, and unclosed quotes.
fn tokenize_shell(input: &str) -> Option<Vec<Token>> {
    if input.is_empty() || input.len() > 8192 {
        return None;
    }
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut tokens = Vec::new();

    while i < len {
        // Skip horizontal whitespace.
        while i < len && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }
        let b = bytes[i];
        match b {
            b'\n' => {
                tokens.push(Token {
                    start: i,
                    end: i + 1,
                    kind: TokenKind::ListSep,
                    needs_target: false,
                });
                i += 1;
            }
            b';' => {
                if i + 1 < len && bytes[i + 1] == b';' {
                    return None; // case terminator
                }
                tokens.push(Token {
                    start: i,
                    end: i + 1,
                    kind: TokenKind::ListSep,
                    needs_target: false,
                });
                i += 1;
            }
            b'(' | b')' | b'{' | b'}' => return None,
            b'#' => return None,
            b'&' => {
                if i + 1 < len && bytes[i + 1] == b'&' {
                    tokens.push(Token {
                        start: i,
                        end: i + 2,
                        kind: TokenKind::ListSep,
                        needs_target: false,
                    });
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
                    // Background job `&`: leave alone.
                    return None;
                }
            }
            b'|' => {
                if i + 1 < len && bytes[i + 1] == b'|' {
                    tokens.push(Token {
                        start: i,
                        end: i + 2,
                        kind: TokenKind::ListSep,
                        needs_target: false,
                    });
                    i += 2;
                } else if i + 1 < len && bytes[i + 1] == b'&' {
                    tokens.push(Token {
                        start: i,
                        end: i + 2,
                        kind: TokenKind::Pipe,
                        needs_target: false,
                    });
                    i += 2;
                } else {
                    tokens.push(Token {
                        start: i,
                        end: i + 1,
                        kind: TokenKind::Pipe,
                        needs_target: false,
                    });
                    i += 1;
                }
            }
            b'<' => {
                let start = i;
                i += 1;
                if i < len && matches!(bytes[i], b'<' | b'>' | b'&') {
                    i += 1;
                    if i < len && bytes[i] == b'<' {
                        i += 1;
                    }
                }
                if &bytes[start..i] == b"<<" || &bytes[start..i] == b"<<<" {
                    return None;
                }
                i = scan_word_tail(bytes, i)?;
                tokens.push(Token {
                    start,
                    end: i,
                    kind: TokenKind::InputRedir,
                    needs_target: false,
                });
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
                    let mut op_end = d + 1;
                    if op_end < len && matches!(bytes[op_end], b'<' | b'>' | b'&') {
                        op_end += 1;
                    }
                    if &bytes[d..op_end] == b"<<" {
                        return None;
                    }
                    let next = scan_word_tail(bytes, op_end)?;
                    tokens.push(Token {
                        start,
                        end: next,
                        kind: TokenKind::InputRedir,
                        needs_target: false,
                    });
                    i = next;
                } else {
                    let next = scan_word_tail(bytes, i)?;
                    tokens.push(Token {
                        start,
                        end: next,
                        kind: TokenKind::Word,
                        needs_target: false,
                    });
                    i = next;
                }
            }
        }
    }

    Some(tokens)
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

fn scan_word_tail(bytes: &[u8], mut i: usize) -> Option<usize> {
    let len = bytes.len();
    while i < len {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' => break,
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i >= len {
                    return None;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                        if i >= len {
                            return None;
                        }
                    }
                    i += 1;
                }
                if i >= len {
                    return None;
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
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

/// Rewrite wrapped check commands (e.g. `cargo check ... 2>&1 | grep ... | head; echo ${PIPESTATUS[0]}`)
/// by wrapping only the inner check command(s) while preserving surrounding pipes, redirections, and list items.
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
        while idx < tokens.len() && tokens[idx].kind != TokenKind::ListSep {
            idx += 1;
        }
        let item = &tokens[item_start..idx];
        // Advance past the separator for the next iteration.
        if idx < tokens.len() && tokens[idx].kind == TokenKind::ListSep {
            idx += 1;
        }
        if item.is_empty() {
            continue;
        }
        // Any control keyword at command position aborts wrapping the whole command.
        if item[0].kind == TokenKind::Word
            && SHELL_KEYWORDS.contains(&&command[item[0].start..item[0].end])
        {
            return None;
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
                TokenKind::Word | TokenKind::ListSep => {
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
            "(cargo check)",
            "if cargo check; then echo ok; fi",
            "cargo check &",
            "cargo check > out.txt --workspace",
            "echo \"cargo check\" | grep cargo",
            "ls -la | head -n 20",
            "'/usr/local/bin/contextstream-mcp' run --tool 'check' -- 'cargo check' 2>&1 | head -n 20",
        ] {
            assert!(
                rewrite_with_cache(&json!({"command": cmd}), bin, &cache).is_none(),
                "expected None for: {cmd}"
            );
        }
    }
}
