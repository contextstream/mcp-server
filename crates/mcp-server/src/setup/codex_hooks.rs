//! Codex `hooks.json` command generation, migration, live shell probing, and
//! trust-state diagnostics.
//!
//! On Windows, Codex executes `PreToolUse` command hooks through either
//! PowerShell (`pwsh.exe` / `powershell.exe -NoProfile -Command <cmd>`) or
//! Command Prompt (`cmd.exe /C "<cmd>"` via raw Win32 command-line args).
//! A bare double-quoted executable path (`"C:\...\contextstream-mcp.exe" hook ...`)
//! is parsed by PowerShell as a string literal followed by an unexpected token
//! `hook`. An unquoted path fails on paths containing spaces or shell
//! metacharacters.
//!
//! We emit a `commandWindows` override (documented at
//! https://learn.chatgpt.com/docs/hooks) that launches `powershell.exe` with a
//! quote-, `$`-, `` ` ``-, and `%`-free inner expression so both outer
//! PowerShell and outer `cmd.exe /C` preserve paths with spaces, apostrophes,
//! parentheses, `&`, and extended-length Win32 prefixes while forwarding raw
//! JSON `stdin`/`stdout` pipes and exact exit codes.

use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::hooks::{
    current_managed_hook_name, hook_command_for_platform, parse_hook_command, CODEX_NOTICE_HOOK,
    MANAGED_HOOK_ARGUMENT,
};

pub const CODEX_HOOK_TIMEOUT_SECS: u64 = 5;
const CODEX_DEFAULT_TIMEOUT_SECS: u64 = 600;
const CODEX_DEFAULT_ADDITIONAL_CONTEXT_LIMIT: u64 = 2500;
const DOCTOR_PROBE_SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440001";

const PS_WRAPPER_OPEN: &str =
    "powershell.exe -NoProfile -NonInteractive -Command \"Set-Variable LASTEXITCODE 1; & ";
const PS_WRAPPER_CLOSE: &str = "; exit (Get-Variable LASTEXITCODE -ValueOnly)\"";

/// Strip Win32 extended-length verbatim path prefixes (`\\?\` and `\\?\UNC\`)
/// that `std::fs::canonicalize` produces on Windows and that PowerShell's `&`
/// call operator in Windows PowerShell 5.1 rejects.
pub fn normalize_windows_executable_path(binary: &str) -> String {
    let trimmed = binary.trim();
    if trimmed.len() >= 8 && trimmed[..8].eq_ignore_ascii_case(r"\\?\UNC\") {
        return format!(r"\\{}", &trimmed[8..]);
    }
    if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    trimmed.to_string()
}

/// Heuristic for whether `path` is a Windows filesystem path even when
/// inspected from a cross-platform unit test.
pub fn looks_like_windows_path(path: &str) -> bool {
    let trimmed = path.trim();
    let bytes = trimmed.as_bytes();
    let has_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    has_drive
        || trimmed.starts_with(r"\\")
        || (trimmed.contains('\\') && trimmed.to_ascii_lowercase().ends_with(".exe"))
}

/// Encode `binary` as a PowerShell expression safe to embed inside an outer
/// double-quoted `"..."` command string that may itself be parsed by either
/// PowerShell or `cmd.exe /C`.
///
/// Output invariants:
/// - Contains zero `"`, `$`, `` ` ``, or `%` characters.
/// - Escapes every apostrophe `'` as `''` inside single-quoted literals.
/// - Encodes `$`, `%`, and `` ` `` (if present in an unusual path) via
///   `[char]36`, `[char]37`, and `[char]96` concatenation so neither outer
///   PowerShell nor outer `cmd.exe` performs variable expansion.
pub fn powershell_safe_path_expression(binary: &str) -> String {
    let normalized = normalize_windows_executable_path(binary);
    if !normalized.contains(['$', '%', '`']) {
        return format!("'{}'", normalized.replace('\'', "''"));
    }

    let mut parts: Vec<String> = Vec::new();
    let mut literal = String::new();
    for ch in normalized.chars() {
        let code = match ch {
            '$' => Some(36u8),
            '%' => Some(37u8),
            '`' => Some(96u8),
            _ => None,
        };
        if let Some(code) = code {
            if !literal.is_empty() {
                parts.push(format!("'{}'", literal.replace('\'', "''")));
                literal.clear();
            }
            parts.push(format!("[char]{code}"));
        } else {
            literal.push(ch);
        }
    }
    if !literal.is_empty() {
        parts.push(format!("'{}'", literal.replace('\'', "''")));
    }
    if parts.len() == 1 {
        parts.pop().unwrap_or_else(|| "''".to_string())
    } else {
        format!("({})", parts.join("+"))
    }
}

/// Build the canonical `commandWindows` override for a ContextStream Codex hook.
pub fn codex_windows_hook_command(binary: &str, hook_name: &str) -> String {
    let expr = powershell_safe_path_expression(binary);
    format!("{PS_WRAPPER_OPEN}{expr} hook {hook_name} {MANAGED_HOOK_ARGUMENT}{PS_WRAPPER_CLOSE}")
}

fn parse_powershell_single_quoted(input: &str) -> Option<(String, &str)> {
    let after_open = input.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = after_open.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '\'' {
            if let Some((_, '\'')) = chars.peek().copied() {
                chars.next();
                out.push('\'');
            } else {
                let rest = &after_open[idx + 1..];
                return Some((out, rest));
            }
        } else {
            out.push(ch);
        }
    }
    None
}

fn parse_powershell_safe_path_expression(input: &str) -> Option<(String, &str)> {
    let trimmed = input.trim_start();
    if trimmed.starts_with('\'') {
        return parse_powershell_single_quoted(trimmed);
    }
    let inner_start = trimmed.strip_prefix('(')?;
    let mut cursor = inner_start.trim_start();
    let mut decoded = String::new();
    loop {
        if cursor.starts_with('\'') {
            let (segment, rest) = parse_powershell_single_quoted(cursor)?;
            decoded.push_str(&segment);
            cursor = rest.trim_start();
        } else if let Some(after_char) = cursor.strip_prefix("[char]") {
            let digits_len = after_char
                .bytes()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if digits_len == 0 {
                return None;
            }
            let code: u32 = after_char[..digits_len].parse().ok()?;
            decoded.push(char::from_u32(code)?);
            cursor = after_char[digits_len..].trim_start();
        } else {
            return None;
        }

        if let Some(after_plus) = cursor.strip_prefix('+') {
            cursor = after_plus.trim_start();
            continue;
        }
        if let Some(after_close) = cursor.strip_prefix(')') {
            return Some((decoded, after_close));
        }
        return None;
    }
}

/// Parse a canonical `codex_windows_hook_command` back into `(binary_path, hook_name)`.
pub fn parse_canonical_codex_windows_hook_command(command: &str) -> Option<(String, String)> {
    let trimmed = command.trim();
    let inner = trimmed
        .strip_prefix(PS_WRAPPER_OPEN)?
        .strip_suffix(PS_WRAPPER_CLOSE)?;
    let (binary, rest) = parse_powershell_safe_path_expression(inner)?;
    if binary.trim().is_empty() {
        return None;
    }
    let mut args = rest.split_whitespace();
    match (args.next(), args.next(), args.next(), args.next()) {
        (Some("hook"), Some(hook_name), Some(MANAGED_HOOK_ARGUMENT), None)
            if is_valid_hook_identifier(hook_name) =>
        {
            Some((binary, hook_name.to_string()))
        }
        _ => None,
    }
}

fn is_valid_hook_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Extract the ContextStream managed hook name from any Codex hook command
/// carrying [`MANAGED_HOOK_ARGUMENT`], including canonical `commandWindows`
/// overrides, legacy 1.0.71 double-quoted commands, and user-authored local
/// Windows workarounds (unquoted paths, `& '...'`, custom PowerShell/`cmd.exe`
/// wrappers).
pub fn managed_codex_hook_name_in_command(command: &str) -> Option<String> {
    if let Some(name) = current_managed_hook_name(command) {
        return Some(name.to_string());
    }
    if let Some((_, name)) = parse_canonical_codex_windows_hook_command(command) {
        return Some(name);
    }

    let trimmed = command.trim();
    let marker_pos = trimmed.find(MANAGED_HOOK_ARGUMENT)?;
    let before_marker = trimmed[..marker_pos].trim_end();
    let after_marker = trimmed[marker_pos + MANAGED_HOOK_ARGUMENT.len()..].trim();
    // Allow trailing quotes or PowerShell exit tails after the managed marker,
    // but reject chained unrelated commands that merely mention the string.
    if !after_marker.is_empty()
        && !matches!(after_marker, "\"" | "'")
        && !after_marker.starts_with("; exit")
        && !after_marker.starts_with(";exit")
    {
        return None;
    }

    let (before_name, hook_name) = before_marker.rsplit_once(char::is_whitespace)?;
    if !is_valid_hook_identifier(hook_name) {
        return None;
    }
    let (_, hook_kw) = before_name
        .trim_end()
        .rsplit_once(char::is_whitespace)
        .unwrap_or(("", before_name.trim_end()));
    if hook_kw != "hook" {
        return None;
    }
    Some(hook_name.to_string())
}

fn parse_unmarked_program_and_rest(command: &str) -> Option<(String, &str)> {
    let trimmed = command.trim();
    if let Some(after_amp) = trimmed.strip_prefix('&') {
        let after_amp = after_amp.trim_start();
        if after_amp.starts_with('\'') {
            let (program, rest) = parse_powershell_single_quoted(after_amp)?;
            return Some((program, rest.trim()));
        }
        if let Some((program, rest)) = parse_hook_command(after_amp) {
            return Some((program.to_string(), rest));
        }
    }
    if trimmed.starts_with('\'') {
        let (program, rest) = parse_powershell_single_quoted(trimmed)?;
        return Some((program, rest.trim()));
    }
    let (program, rest) = parse_hook_command(trimmed)?;
    Some((program.to_string(), rest))
}

fn parse_unmarked_rsplit_hook_command(command: &str) -> Option<(String, &str)> {
    let (before_hook, hook_tail) = command.trim().rsplit_once(" hook ")?;
    let hook_name = hook_tail.trim();
    if hook_name.split_whitespace().count() != 1 {
        return None;
    }
    let mut prog = before_hook.trim();
    if let Some(after_amp) = prog.strip_prefix('&') {
        prog = after_amp.trim_start();
    }
    if prog.len() >= 2
        && ((prog.starts_with('\'') && prog.ends_with('\''))
            || (prog.starts_with('"') && prog.ends_with('"')))
    {
        let quote = prog.as_bytes()[0] as char;
        let inner = &prog[1..prog.len() - 1];
        if quote == '\'' && inner.contains("''") {
            return Some((inner.replace("''", "'"), hook_name));
        }
        return Some((inner.to_string(), hook_name));
    }
    Some((prog.to_string(), hook_name))
}

fn program_matches_managed_binary(program: &str, managed_binary: &Path, windows: bool) -> bool {
    let managed_str = managed_binary.to_string_lossy();
    if windows {
        normalize_windows_executable_path(program)
            .eq_ignore_ascii_case(&normalize_windows_executable_path(&managed_str))
    } else {
        program == managed_str
    }
}

fn is_legacy_unmarked_codex_command(
    command: &str,
    managed_binary: &Path,
    windows: bool,
) -> Option<String> {
    if let Some((program, rest)) = parse_unmarked_program_and_rest(command) {
        let mut args = rest.split_whitespace();
        if let (Some("hook"), Some(hook_name), None) = (args.next(), args.next(), args.next()) {
            if is_valid_hook_identifier(hook_name)
                && program_matches_managed_binary(&program, managed_binary, windows)
            {
                return Some(hook_name.to_string());
            }
        }
    }
    let (program, hook_name) = parse_unmarked_rsplit_hook_command(command)?;
    if !is_valid_hook_identifier(hook_name) {
        return None;
    }
    program_matches_managed_binary(&program, managed_binary, windows).then(|| hook_name.to_string())
}

pub fn owned_codex_hook_name_in_command_for_platform(
    command: &str,
    managed_binary: &Path,
    windows: bool,
) -> Option<String> {
    managed_codex_hook_name_in_command(command)
        .or_else(|| is_legacy_unmarked_codex_command(command, managed_binary, windows))
}

pub fn is_owned_codex_hook_command_for_platform(
    command: &str,
    managed_binary: &Path,
    windows: bool,
) -> bool {
    owned_codex_hook_name_in_command_for_platform(command, managed_binary, windows).is_some()
}

/// Managed hook names present on a Codex handler object across `command`,
/// `commandWindows`, and `command_windows`.
pub fn codex_handler_managed_hook_names(handler: &Value) -> Vec<String> {
    let Some(object) = handler.as_object() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["command", "commandWindows", "command_windows"] {
        if let Some(name) = object
            .get(key)
            .and_then(Value::as_str)
            .and_then(managed_codex_hook_name_in_command)
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

pub fn is_owned_codex_notice_handler_for_platform(
    handler: &Value,
    managed_binary: &Path,
    windows: bool,
) -> bool {
    let Some(object) = handler.as_object() else {
        return false;
    };
    ["command", "commandWindows", "command_windows"]
        .into_iter()
        .any(|key| {
            object
                .get(key)
                .and_then(Value::as_str)
                .and_then(|cmd| {
                    owned_codex_hook_name_in_command_for_platform(cmd, managed_binary, windows)
                })
                .as_deref()
                == Some(CODEX_NOTICE_HOOK)
        })
}

pub fn is_codex_notice_hook_group_for_platform(
    group: &Value,
    managed_binary: &Path,
    windows: bool,
) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                is_owned_codex_notice_handler_for_platform(handler, managed_binary, windows)
            })
        })
}

/// Build the ContextStream `PreToolUse` handler object for Codex `hooks.json`.
///
/// On Windows (or when `binary` is a Windows path), always emits both `command`
/// and `commandWindows`. On POSIX, preserves an existing canonical
/// `commandWindows` override if the user shares `~/.codex/hooks.json` across
/// OSes (e.g. WSL / dotfiles).
pub fn codex_notice_hook_handler_for_platform(
    binary: &str,
    windows: bool,
    preserved_posix_windows_override: Option<&str>,
) -> Value {
    let emit_windows = windows || looks_like_windows_path(binary);
    let command_binary = if emit_windows {
        normalize_windows_executable_path(binary)
    } else {
        binary.to_string()
    };
    let mut handler = serde_json::Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert(
        "command".to_string(),
        Value::String(hook_command_for_platform(
            &command_binary,
            CODEX_NOTICE_HOOK,
            emit_windows,
        )),
    );
    if emit_windows {
        handler.insert(
            "commandWindows".to_string(),
            Value::String(codex_windows_hook_command(binary, CODEX_NOTICE_HOOK)),
        );
    } else if let Some(override_cmd) = preserved_posix_windows_override {
        handler.insert(
            "commandWindows".to_string(),
            Value::String(override_cmd.to_string()),
        );
    }
    handler.insert(
        "timeout".to_string(),
        Value::Number(CODEX_HOOK_TIMEOUT_SECS.into()),
    );
    Value::Object(handler)
}

pub fn codex_notice_hook_group_for_platform(
    binary: &str,
    windows: bool,
    preserved_posix_windows_override: Option<&str>,
) -> Value {
    json!({
        "hooks": [codex_notice_hook_handler_for_platform(
            binary,
            windows,
            preserved_posix_windows_override,
        )]
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHookUpdateKind {
    Unchanged,
    InstalledNew,
    MigratedExisting,
}

fn existing_canonical_windows_override_in(
    groups: &[Value],
    managed_binary: &Path,
    windows: bool,
) -> Option<String> {
    for group in groups {
        let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for handler in handlers {
            if !is_owned_codex_notice_handler_for_platform(handler, managed_binary, windows) {
                continue;
            }
            if let Some(cmd_win) = handler
                .get("commandWindows")
                .or_else(|| handler.get("command_windows"))
                .and_then(Value::as_str)
            {
                if let Some((_, hook_name)) = parse_canonical_codex_windows_hook_command(cmd_win) {
                    if hook_name == CODEX_NOTICE_HOOK {
                        return Some(cmd_win.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Add or migrate ContextStream's `mid-run-notices` hook in a Codex `hooks.json`
/// document while preserving all unrelated events, groups, and user handlers.
pub fn update_codex_hooks_document_for_platform(
    existing: &Value,
    path: &Path,
    binary: &str,
    managed_binary: &Path,
    windows: bool,
) -> Result<(Value, CodexHookUpdateKind)> {
    let mut root = match existing {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => anyhow::bail!(
            "Refusing to modify {}: it is not a JSON object.",
            path.display()
        ),
    };
    let mut hooks = match root.get("hooks") {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(_) => anyhow::bail!(
            "Refusing to modify {}: \"hooks\" is not a JSON object.",
            path.display()
        ),
    };

    let mut had_existing_notice_hook = false;
    let mut preserved_posix_override = None;

    if let Some(pre_tool_use) = hooks.get("PreToolUse") {
        let Some(groups) = pre_tool_use.as_array() else {
            anyhow::bail!(
                "Refusing to modify {}: \"PreToolUse\" is not an array.",
                path.display()
            );
        };
        had_existing_notice_hook = groups
            .iter()
            .any(|group| is_codex_notice_hook_group_for_platform(group, managed_binary, windows));
        if !windows && !looks_like_windows_path(binary) {
            preserved_posix_override =
                existing_canonical_windows_override_in(groups, managed_binary, windows);
        }
    }

    let new_group =
        codex_notice_hook_group_for_platform(binary, windows, preserved_posix_override.as_deref());

    match hooks.get_mut("PreToolUse") {
        Some(Value::Array(groups)) => {
            let stripped = strip_owned_codex_notice_handlers(groups, managed_binary, windows);
            let mut next_groups = stripped;
            next_groups.push(new_group);
            *groups = next_groups;
        }
        None => {
            hooks.insert("PreToolUse".to_string(), Value::Array(vec![new_group]));
        }
        Some(_) => unreachable!("validated above"),
    }

    root.insert("hooks".to_string(), Value::Object(hooks));
    let updated = Value::Object(root);
    let kind = if &updated == existing {
        CodexHookUpdateKind::Unchanged
    } else if had_existing_notice_hook {
        CodexHookUpdateKind::MigratedExisting
    } else {
        CodexHookUpdateKind::InstalledNew
    };
    Ok((updated, kind))
}

fn strip_owned_codex_notice_handlers(
    groups: &[Value],
    managed_binary: &Path,
    windows: bool,
) -> Vec<Value> {
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let Some(mut group_obj) = group.as_object().cloned() else {
            out.push(group.clone());
            continue;
        };
        let Some(Value::Array(handlers)) = group_obj.get("hooks").cloned() else {
            out.push(Value::Object(group_obj));
            continue;
        };
        let before = handlers.len();
        let kept: Vec<Value> = handlers
            .into_iter()
            .filter(|handler| {
                !is_owned_codex_notice_handler_for_platform(handler, managed_binary, windows)
            })
            .collect();
        let removed_owned = kept.len() != before;
        if removed_owned && kept.is_empty() {
            continue;
        }
        group_obj.insert("hooks".to_string(), Value::Array(kept));
        out.push(Value::Object(group_obj));
    }
    out
}

/// Remove only ContextStream's `mid-run-notices` hook from a Codex `hooks.json`
/// document, leaving unrelated user hooks intact.
pub fn remove_codex_notice_hooks_from_document_for_platform(
    existing: &Value,
    path: &Path,
    managed_binary: &Path,
    windows: bool,
) -> Result<Value> {
    let mut root = match existing {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => anyhow::bail!(
            "Refusing to modify {}: it is not a JSON object.",
            path.display()
        ),
    };
    let mut hooks = match root.get("hooks") {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(_) => anyhow::bail!(
            "Refusing to modify {}: \"hooks\" is not a JSON object.",
            path.display()
        ),
    };
    if let Some(Value::Array(groups)) = hooks.get("PreToolUse").cloned() {
        let was_empty = groups.is_empty();
        let stripped = strip_owned_codex_notice_handlers(&groups, managed_binary, windows);
        if stripped.is_empty() && !was_empty {
            hooks.remove("PreToolUse");
        } else {
            hooks.insert("PreToolUse".to_string(), Value::Array(stripped));
        }
    }
    root.insert("hooks".to_string(), Value::Object(hooks));
    Ok(Value::Object(root))
}

/// Locate the single installed ContextStream `mid-run-notices` handler and its
/// `(group_index, handler_index, group, handler)` coordinates in `hooks.PreToolUse`.
pub fn find_installed_codex_notice_handler<'a>(
    value: &'a Value,
    managed_binary: &Path,
    windows: bool,
) -> std::result::Result<(usize, usize, &'a Value, &'a Value), String> {
    let groups = value
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array)
        .ok_or_else(|| "no PreToolUse hooks".to_string())?;

    let mut matches = Vec::new();
    for (group_idx, group) in groups.iter().enumerate() {
        let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for (handler_idx, handler) in handlers.iter().enumerate() {
            if is_owned_codex_notice_handler_for_platform(handler, managed_binary, windows) {
                matches.push((group_idx, handler_idx, group, handler));
            }
        }
    }

    match matches.as_slice() {
        [] => Err("the mid-run notice hook is not installed".to_string()),
        [single] => Ok(*single),
        _ => Err(format!(
            "found {} ContextStream mid-run notice hooks in PreToolUse; expected 1",
            matches.len()
        )),
    }
}

/// Validate ContextStream's Codex `hooks.json` schema and platform command shape.
pub fn validate_codex_notice_hooks_for_platform(
    value: &Value,
    managed_binary: &Path,
    windows: bool,
) -> std::result::Result<usize, String> {
    let (_, _, group, handler) =
        find_installed_codex_notice_handler(value, managed_binary, windows)?;

    if group
        .get("matcher")
        .is_some_and(|matcher| !matcher.is_null())
    {
        return Err("mid-run-notices PreToolUse group must not restrict matcher".to_string());
    }

    let obj = handler
        .as_object()
        .ok_or_else(|| "mid-run-notices hook entry is not a JSON object".to_string())?;

    if obj.get("type").and_then(Value::as_str) != Some("command") {
        return Err("mid-run-notices hook entry must have type \"command\"".to_string());
    }
    if obj.get("timeout").and_then(Value::as_u64) != Some(CODEX_HOOK_TIMEOUT_SECS) {
        return Err(format!(
            "mid-run-notices hook entry must have timeout {CODEX_HOOK_TIMEOUT_SECS}"
        ));
    }
    if obj.get("async").and_then(Value::as_bool) == Some(true) {
        return Err("mid-run-notices hook entry must run synchronously".to_string());
    }
    if obj.contains_key("command_windows") {
        return Err(
            "mid-run-notices hook entry uses alias \"command_windows\"; expected \"commandWindows\""
                .to_string(),
        );
    }

    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "mid-run-notices hook entry is missing \"command\"".to_string())?;
    if current_managed_hook_name(command) != Some(CODEX_NOTICE_HOOK) {
        return Err(
            "mid-run-notices \"command\" is outdated or missing the managed marker".to_string(),
        );
    }

    match obj.get("commandWindows") {
        Some(Value::String(cmd_win)) => match parse_canonical_codex_windows_hook_command(cmd_win) {
            Some((_, ref hook_name)) if hook_name == CODEX_NOTICE_HOOK => {}
            _ => {
                return Err(
                        "mid-run-notices \"commandWindows\" is outdated or not properly escaped for Windows shells"
                            .to_string(),
                    );
            }
        },
        Some(_) => {
            return Err("mid-run-notices \"commandWindows\" must be a string".to_string());
        }
        None if windows => {
            return Err(
                "mid-run-notices hook is missing \"commandWindows\" (bare quoted paths fail in Windows PowerShell)"
                    .to_string(),
            );
        }
        None => {}
    }

    Ok(1)
}

/// Return the command string Codex will actually execute on `windows`.
pub fn effective_codex_hook_command_for_platform(handler: &Value, windows: bool) -> Option<&str> {
    let obj = handler.as_object()?;
    if windows {
        obj.get("commandWindows")
            .or_else(|| obj.get("command_windows"))
            .and_then(Value::as_str)
            .or_else(|| obj.get("command").and_then(Value::as_str))
    } else {
        obj.get("command").and_then(Value::as_str)
    }
}

/// Compute the exact `sha256:<hex>` hook fingerprint that `openai/codex`
/// (`discovery.rs::hook_hash` + `fingerprint.rs::version_for_toml`) stores under
/// `[hooks.state]` in `~/.codex/config.toml`.
pub fn codex_hook_identity_hash_for_platform(
    group: &Value,
    handler: &Value,
    windows: bool,
) -> Option<String> {
    let obj = handler.as_object()?;
    let effective_command = effective_codex_hook_command_for_platform(handler, windows)?.trim();
    if effective_command.is_empty() {
        return None;
    }
    let timeout_sec = obj
        .get("timeout")
        .and_then(Value::as_u64)
        .filter(|timeout| *timeout > 0)
        .unwrap_or(CODEX_DEFAULT_TIMEOUT_SECS);
    let runs_async = obj.get("async").and_then(Value::as_bool).unwrap_or(false);

    let mut normalized_handler: BTreeMap<&'static str, Value> = BTreeMap::new();
    if let Some(limit) = obj
        .get("additionalContextLimit")
        .and_then(Value::as_u64)
        .filter(|limit| *limit != CODEX_DEFAULT_ADDITIONAL_CONTEXT_LIMIT)
    {
        normalized_handler.insert("additionalContextLimit", Value::Number(limit.into()));
    }
    normalized_handler.insert("async", Value::Bool(runs_async));
    normalized_handler.insert(
        "command",
        Value::String(effective_codex_hook_command_for_platform(handler, windows)?.to_string()),
    );
    if let Some(status_message) = obj.get("statusMessage").and_then(Value::as_str) {
        normalized_handler.insert("statusMessage", Value::String(status_message.to_string()));
    }
    normalized_handler.insert("timeout", Value::Number(timeout_sec.into()));
    normalized_handler.insert("type", Value::String("command".to_string()));

    let handler_value = serde_json::to_value(normalized_handler).ok()?;
    let mut normalized_identity: BTreeMap<&'static str, Value> = BTreeMap::new();
    normalized_identity.insert("event_name", Value::String("pre_tool_use".to_string()));
    normalized_identity.insert("hooks", Value::Array(vec![handler_value]));
    if let Some(matcher) = group.get("matcher").and_then(Value::as_str) {
        normalized_identity.insert("matcher", Value::String(matcher.to_string()));
    }

    let canonical_bytes = serde_json::to_vec(&normalized_identity).ok()?;
    let digest = Sha256::digest(&canonical_bytes);
    Some(format!("sha256:{}", hex::encode(digest)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexHookTrustState {
    Trusted {
        current_hash: String,
    },
    Untrusted {
        current_hash: String,
    },
    Modified {
        current_hash: String,
        trusted_hash: String,
    },
    DisabledByHookState {
        current_hash: String,
    },
    DisabledByFeatureFlag {
        current_hash: String,
    },
}

pub fn codex_config_toml_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex").join("config.toml"))
}

fn candidate_state_keys(hooks_path: &Path, group_idx: usize, handler_idx: usize) -> Vec<String> {
    let suffix = format!(":pre_tool_use:{group_idx}:{handler_idx}");
    let mut keys = vec![format!("{}{suffix}", hooks_path.display())];
    if let Ok(canonical) = std::fs::canonicalize(hooks_path) {
        let normalized = normalize_windows_executable_path(&canonical.to_string_lossy());
        let candidate = format!("{normalized}{suffix}");
        if !keys.contains(&candidate) {
            keys.push(candidate);
        }
    }
    keys
}

/// Inspect `~/.codex/config.toml` (`[features].codex_hooks` and `[hooks.state]`)
/// for ContextStream's installed `mid-run-notices` hook.
pub fn inspect_codex_hook_trust_for_platform(
    hooks_path: &Path,
    hooks_doc: &Value,
    config_toml_path: Option<&Path>,
    managed_binary: &Path,
    windows: bool,
) -> std::result::Result<CodexHookTrustState, String> {
    let (group_idx, handler_idx, group, handler) =
        find_installed_codex_notice_handler(hooks_doc, managed_binary, windows)?;
    let current_hash = codex_hook_identity_hash_for_platform(group, handler, windows)
        .ok_or_else(|| "could not fingerprint Codex mid-run-notices hook".to_string())?;

    let Some(config_path) = config_toml_path.filter(|path| path.exists()) else {
        return Ok(CodexHookTrustState::Untrusted { current_hash });
    };
    let raw = std::fs::read_to_string(config_path).map_err(|error| {
        format!(
            "could not read Codex config {}: {error}",
            config_path.display()
        )
    })?;
    let doc = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
        format!(
            "Codex config {} does not parse as TOML: {error}",
            config_path.display()
        )
    })?;

    if doc
        .get("features")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|features| features.get("codex_hooks"))
        .and_then(toml_edit::Item::as_bool)
        == Some(false)
    {
        return Ok(CodexHookTrustState::DisabledByFeatureFlag { current_hash });
    }

    let state_table = doc
        .get("hooks")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml_edit::Item::as_table_like);

    let keys = candidate_state_keys(hooks_path, group_idx, handler_idx);
    let matched_entry = state_table.and_then(|table| {
        keys.iter()
            .find_map(|key| table.get(key).and_then(toml_edit::Item::as_table_like))
            .or_else(|| {
                if cfg!(windows) || windows {
                    table.iter().find_map(|(k, v)| {
                        keys.iter()
                            .any(|expected| k.eq_ignore_ascii_case(expected))
                            .then(|| v.as_table_like())
                            .flatten()
                    })
                } else {
                    None
                }
            })
    });

    let Some(entry) = matched_entry else {
        return Ok(CodexHookTrustState::Untrusted { current_hash });
    };

    if entry.get("enabled").and_then(toml_edit::Item::as_bool) == Some(false) {
        return Ok(CodexHookTrustState::DisabledByHookState { current_hash });
    }

    match entry.get("trusted_hash").and_then(toml_edit::Item::as_str) {
        Some(trusted_hash) if trusted_hash == current_hash => {
            Ok(CodexHookTrustState::Trusted { current_hash })
        }
        Some(trusted_hash) => Ok(CodexHookTrustState::Modified {
            current_hash,
            trusted_hash: trusted_hash.to_string(),
        }),
        None => Ok(CodexHookTrustState::Untrusted { current_hash }),
    }
}

/// Shell runtime variants matching `openai/codex` `command_runner.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexShellKind {
    UnixSh,
    PowerShell,
    #[cfg_attr(not(test), allow(dead_code))]
    Pwsh,
    CmdExe,
}

impl CodexShellKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::UnixSh => "POSIX sh",
            Self::PowerShell => "Windows PowerShell",
            Self::Pwsh => "PowerShell 7 (pwsh)",
            Self::CmdExe => "Command Prompt (cmd.exe)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexHookProbeOutput {
    #[cfg_attr(not(test), allow(dead_code))]
    pub parsed_stdout: Value,
    #[cfg_attr(not(test), allow(dead_code))]
    pub raw_stdout: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub raw_stderr: String,
}

/// Build a sample Codex `PreToolUse` JSON payload matching Codex's published
/// hook schema (`turn_id` and `model` identify the caller as Codex).
pub fn sample_codex_pre_tool_use_input(cwd: &Path, session_id: &str) -> Value {
    json!({
        "session_id": session_id,
        "turn_id": "doctor-probe-turn",
        "transcript_path": null,
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "model": "gpt-5.4",
        "permission_mode": "default",
        "tool_name": "Bash",
        "tool_input": {
            "command": "echo contextstream-doctor-probe"
        },
        "tool_use_id": "doctor-probe-call-1"
    })
}

/// Validate `stdout` against Codex's `PreToolUse` hook output contract.
pub fn validate_codex_pre_tool_use_stdout(stdout: &str) -> std::result::Result<Value, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    let parsed: Value = serde_json::from_str(trimmed).map_err(|error| {
        format!(
            "stdout is not valid JSON ({error}): {}",
            preview_text(trimmed)
        )
    })?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| "stdout JSON must be an object".to_string())?;

    const ALLOWED_TOP_LEVEL: &[&str] = &[
        "continue",
        "decision",
        "hookSpecificOutput",
        "reason",
        "stopReason",
        "suppressOutput",
        "systemMessage",
    ];
    for key in obj.keys() {
        if !ALLOWED_TOP_LEVEL.contains(&key.as_str()) {
            return Err(format!(
                "stdout contains unsupported Codex PreToolUse field \"{key}\""
            ));
        }
    }
    if obj.get("continue") == Some(&Value::Bool(false))
        || obj.contains_key("stopReason")
        || obj.contains_key("suppressOutput")
    {
        return Err(
            "stdout sets a universal stop/suppress field that Codex rejects on PreToolUse"
                .to_string(),
        );
    }
    if let Some(specific) = obj.get("hookSpecificOutput") {
        let specific_obj = specific
            .as_object()
            .ok_or_else(|| "hookSpecificOutput must be a JSON object".to_string())?;
        const ALLOWED_SPECIFIC: &[&str] = &[
            "hookEventName",
            "additionalContext",
            "permissionDecision",
            "permissionDecisionReason",
            "updatedInput",
        ];
        for key in specific_obj.keys() {
            if !ALLOWED_SPECIFIC.contains(&key.as_str()) {
                return Err(format!(
                    "hookSpecificOutput contains unsupported PreToolUse field \"{key}\""
                ));
            }
        }
        if specific_obj.get("hookEventName").and_then(Value::as_str) != Some("PreToolUse") {
            return Err("hookSpecificOutput.hookEventName must equal \"PreToolUse\"".to_string());
        }
    }

    Ok(parsed)
}

fn preview_text(text: &str) -> String {
    let single_line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if single_line.chars().count() <= 180 {
        single_line
    } else {
        let head: String = single_line.chars().take(180).collect();
        format!("{head}...")
    }
}

fn build_shell_command(shell: CodexShellKind, command_str: &str) -> Command {
    match shell {
        CodexShellKind::UnixSh => {
            let sh = std::env::var_os("SHELL")
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .unwrap_or_else(|| PathBuf::from("/bin/sh"));
            let mut cmd = Command::new(sh);
            cmd.arg("-c").arg(command_str);
            cmd
        }
        CodexShellKind::PowerShell => {
            let exe =
                which::which("powershell.exe").unwrap_or_else(|_| PathBuf::from("powershell.exe"));
            let mut cmd = Command::new(exe);
            cmd.args(["-NoProfile", "-Command", command_str]);
            cmd
        }
        CodexShellKind::Pwsh => {
            let exe = which::which("pwsh").unwrap_or_else(|_| PathBuf::from("pwsh"));
            let mut cmd = Command::new(exe);
            cmd.args(["-NoProfile", "-Command", command_str]);
            cmd
        }
        CodexShellKind::CmdExe => {
            let comspec = std::env::var_os("COMSPEC")
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .or_else(|| which::which("cmd.exe").ok())
                .unwrap_or_else(|| PathBuf::from("cmd.exe"));
            let mut cmd = Command::new(comspec);
            cmd.arg("/C");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.raw_arg(format!("\"{command_str}\""));
            }
            #[cfg(not(windows))]
            {
                cmd.arg(command_str);
            }
            cmd
        }
    }
}

/// Execute a Codex hook command through the specified shell with `input_json`
/// piped to `stdin`, enforcing `timeout`, exit code `0`, and Codex's
/// `PreToolUse` JSON output contract.
pub fn run_codex_hook_command_in_shell(
    shell: CodexShellKind,
    command_str: &str,
    cwd: &Path,
    input_json: &Value,
    timeout: Duration,
    extra_env: &[(&str, &OsStr)],
    disable_side_effects: bool,
) -> std::result::Result<CodexHookProbeOutput, String> {
    let mut cmd = build_shell_command(shell, command_str);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if disable_side_effects {
        cmd.env("CONTEXTSTREAM_MID_RUN_NOTICES", "0")
            .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
            .env("CONTEXTSTREAM_WATCH", "0");
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| format!("failed to spawn {} for hook probe: {error}", shell.label()))?;

    let mut payload = serde_json::to_vec(input_json)
        .map_err(|error| format!("failed to serialize probe JSON: {error}"))?;
    payload.push(b'\n');
    if let Some(mut stdin) = child.stdin.take() {
        // Ignore broken pipe here; exit status / stderr below reports why the
        // command exited before reading stdin.
        let _ = stdin.write_all(&payload);
    }

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "missing child stdout pipe".to_string())?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "missing child stderr pipe".to_string())?;

    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.take(256 * 1024).read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.take(256 * 1024).read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(format!(
                        "hook command timed out after {:.1}s in {}",
                        timeout.as_secs_f64(),
                        shell.label()
                    ));
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed while waiting on {} hook probe: {error}",
                    shell.label()
                ));
            }
        }
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    let raw_stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let raw_stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if !status.success() {
        let code_label = status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string());
        let detail = if !raw_stderr.trim().is_empty() {
            preview_text(&raw_stderr)
        } else if !raw_stdout.trim().is_empty() {
            preview_text(&raw_stdout)
        } else {
            "no output".to_string()
        };
        return Err(format!(
            "hook command exited with status {code_label} in {}: {detail}",
            shell.label()
        ));
    }

    let parsed_stdout = validate_codex_pre_tool_use_stdout(&raw_stdout)
        .map_err(|reason| format!("invalid hook output in {}: {reason}", shell.label()))?;

    Ok(CodexHookProbeOutput {
        parsed_stdout,
        raw_stdout,
        raw_stderr,
    })
}

/// Live side-effect-free `doctor` probe for the installed Codex `PreToolUse`
/// hook. Runs the effective command (`commandWindows` on Windows, `command`
/// otherwise) through the actual platform shell(s) with sample Codex JSON on
/// `stdin`.
pub fn probe_installed_codex_hook_for_doctor(
    hooks_path: &Path,
    hooks_doc: &Value,
    managed_binary: &Path,
) -> std::result::Result<(), String> {
    let (_, _, _, handler) =
        find_installed_codex_notice_handler(hooks_doc, managed_binary, cfg!(windows))?;
    let effective = effective_codex_hook_command_for_platform(handler, cfg!(windows))
        .ok_or_else(|| "mid-run-notices hook has no effective command".to_string())?;
    let timeout_secs = handler
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(CODEX_HOOK_TIMEOUT_SECS)
        .clamp(1, 10);
    let timeout = Duration::from_secs(timeout_secs);

    let probe_cwd = hooks_path
        .parent()
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir);
    let input = sample_codex_pre_tool_use_input(&probe_cwd, DOCTOR_PROBE_SESSION_ID);

    let shells: &[CodexShellKind] = if cfg!(windows) {
        &[CodexShellKind::PowerShell, CodexShellKind::CmdExe]
    } else {
        &[CodexShellKind::UnixSh]
    };

    for shell in shells {
        run_codex_hook_command_in_shell(*shell, effective, &probe_cwd, &input, timeout, &[], true)?;
    }
    Ok(())
}

/// User-facing guidance when `setup`, `update-hooks`, or `doctor --repair`
/// installs or migrates Codex's `~/.codex/hooks.json`.
pub fn codex_post_update_guidance(
    hooks_path: &Path,
    updated_doc: &Value,
    update_kind: CodexHookUpdateKind,
    managed_binary: &Path,
    dry_run: bool,
) -> Option<String> {
    match update_kind {
        CodexHookUpdateKind::Unchanged => None,
        CodexHookUpdateKind::MigratedExisting if dry_run => Some(format!(
            "Codex hook in {} would be migrated; after applying, run /hooks in Codex CLI to review/approve the updated hook and reload your Codex session.",
            hooks_path.display()
        )),
        CodexHookUpdateKind::InstalledNew if dry_run => Some(format!(
            "Codex hook would be written to {}; after applying, run /hooks in Codex CLI if prompted to approve it and reload your Codex session.",
            hooks_path.display()
        )),
        CodexHookUpdateKind::MigratedExisting | CodexHookUpdateKind::InstalledNew => {
            let trust = inspect_codex_hook_trust_for_platform(
                hooks_path,
                updated_doc,
                codex_config_toml_path().as_deref(),
                managed_binary,
                cfg!(windows),
            )
            .ok();
            let action_prefix = if update_kind == CodexHookUpdateKind::MigratedExisting {
                format!("Migrated Codex hook in {}.", hooks_path.display())
            } else {
                format!("Installed Codex hook in {}.", hooks_path.display())
            };
            match trust {
                Some(CodexHookTrustState::Trusted { .. }) => Some(format!(
                    "{action_prefix} Reload your Codex session to pick up the hook."
                )),
                Some(CodexHookTrustState::Modified { .. }) => Some(format!(
                    "{action_prefix} Because the hook command changed, run /hooks in Codex CLI to review and approve the updated hook, then reload your Codex session."
                )),
                Some(CodexHookTrustState::DisabledByFeatureFlag { .. }) => Some(format!(
                    "{action_prefix} Enable `codex_hooks = true` under `[features]` in ~/.codex/config.toml, review via /hooks in Codex CLI, and reload your Codex session."
                )),
                Some(CodexHookTrustState::DisabledByHookState { .. }) => Some(format!(
                    "{action_prefix} Re-enable the hook via /hooks in Codex CLI and reload your Codex session."
                )),
                Some(CodexHookTrustState::Untrusted { .. }) | None => Some(format!(
                    "{action_prefix} Run /hooks in Codex CLI to review/approve the hook and reload your Codex session."
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_command_escapes_spaces_apostrophes_metacharacters_and_verbatim_paths() {
        let cases = [
            (
                r"C:\Users\Jane Doe\.contextstream\bin\contextstream-mcp.exe",
                r"C:\Users\Jane Doe\.contextstream\bin\contextstream-mcp.exe",
            ),
            (
                r"C:\Users\O'Brien\Program Files (x86)\ContextStream & Co\contextstream-mcp.exe",
                r"C:\Users\O'Brien\Program Files (x86)\ContextStream & Co\contextstream-mcp.exe",
            ),
            (
                r"C:\Users\Dev$1%TEMP%`tick\O'Brien\contextstream-mcp.exe",
                r"C:\Users\Dev$1%TEMP%`tick\O'Brien\contextstream-mcp.exe",
            ),
            (
                r"\\?\C:\Users\O'Brien\AppData\Local\contextstream-mcp.exe",
                r"C:\Users\O'Brien\AppData\Local\contextstream-mcp.exe",
            ),
            (
                r"\\?\UNC\fileserver\Team Share\O'Brien\contextstream-mcp.exe",
                r"\\fileserver\Team Share\O'Brien\contextstream-mcp.exe",
            ),
        ];

        for (raw_input, expected_decoded) in cases {
            let expr = powershell_safe_path_expression(raw_input);
            assert!(
                !expr.contains(['"', '$', '%', '`']),
                "unsafe outer-shell character leaked in {expr}"
            );

            let cmd = codex_windows_hook_command(raw_input, CODEX_NOTICE_HOOK);
            assert!(
                !cmd.contains(['$', '%', '`']),
                "outer-shell expansion character leaked in {cmd}"
            );
            assert_eq!(
                cmd.matches('"').count(),
                2,
                "commandWindows must have only the outer -Command quotes: {cmd}"
            );

            let (decoded, hook_name) = parse_canonical_codex_windows_hook_command(&cmd)
                .unwrap_or_else(|| panic!("failed to parse generated command: {cmd}"));
            assert_eq!(decoded, expected_decoded);
            assert_eq!(hook_name, CODEX_NOTICE_HOOK);
            assert_eq!(
                managed_codex_hook_name_in_command(&cmd).as_deref(),
                Some(CODEX_NOTICE_HOOK)
            );
        }
    }

    #[test]
    fn migration_replaces_broken_1_0_71_and_local_workarounds_while_keeping_user_hooks() {
        let managed = Path::new(r"C:\Users\O'Brien\.contextstream\bin\contextstream-mcp.exe");
        let binary_str = managed.to_str().unwrap();
        let hooks_path = Path::new(r"C:\Users\O'Brien\.codex\hooks.json");

        let legacy_1_0_71 = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "echo audit" }]
                    },
                    {
                        "hooks": [{
                            "type": "command",
                            "command": "\"C:\\Users\\O'Brien\\.contextstream\\bin\\contextstream-mcp.exe\" hook mid-run-notices --contextstream-managed-hook=v1",
                            "timeout": 5
                        }]
                    }
                ],
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "echo start" }] }
                ]
            }
        });

        assert!(
            validate_codex_notice_hooks_for_platform(&legacy_1_0_71, managed, true).is_err(),
            "1.0.71 Windows entry without commandWindows must fail validation"
        );

        let (migrated, kind) = update_codex_hooks_document_for_platform(
            &legacy_1_0_71,
            hooks_path,
            binary_str,
            managed,
            true,
        )
        .expect("migrate 1.0.71");
        assert_eq!(kind, CodexHookUpdateKind::MigratedExisting);
        assert_eq!(
            validate_codex_notice_hooks_for_platform(&migrated, managed, true),
            Ok(1)
        );
        let groups = migrated["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "Bash");
        assert_eq!(
            migrated["hooks"]["SessionStart"],
            legacy_1_0_71["hooks"]["SessionStart"]
        );

        // Re-running update is a strict no-op and preserves commandWindows.
        let (rerun, rerun_kind) = update_codex_hooks_document_for_platform(
            &migrated, hooks_path, binary_str, managed, true,
        )
        .expect("idempotent rerun");
        assert_eq!(rerun_kind, CodexHookUpdateKind::Unchanged);
        assert_eq!(rerun, migrated);

        // User's local unquoted-path workaround in commandWindows or command is also migrated cleanly.
        let user_workaround = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [
                        { "type": "command", "command": "echo keep-me" },
                        {
                            "type": "command",
                            "command": "C:\\Users\\Alice\\contextstream-mcp.exe hook mid-run-notices --contextstream-managed-hook=v1",
                            "commandWindows": "C:\\Users\\Alice\\contextstream-mcp.exe hook mid-run-notices --contextstream-managed-hook=v1",
                            "timeout": 5
                        }
                    ]
                }]
            }
        });
        assert!(
            validate_codex_notice_hooks_for_platform(&user_workaround, managed, true).is_err(),
            "unquoted local workaround must trigger migration to escaped commandWindows"
        );
        let (migrated_workaround, wk_kind) = update_codex_hooks_document_for_platform(
            &user_workaround,
            hooks_path,
            binary_str,
            managed,
            true,
        )
        .expect("migrate workaround");
        assert_eq!(wk_kind, CodexHookUpdateKind::MigratedExisting);
        assert_eq!(
            validate_codex_notice_hooks_for_platform(&migrated_workaround, managed, true),
            Ok(1)
        );
        let wk_groups = migrated_workaround["hooks"]["PreToolUse"]
            .as_array()
            .unwrap();
        assert_eq!(
            wk_groups[0]["hooks"],
            json!([{ "type": "command", "command": "echo keep-me" }]),
            "sibling user handler in the same group must survive migration"
        );
    }

    #[test]
    fn posix_refresh_preserves_existing_canonical_command_windows_override() {
        let managed = Path::new("/home/alice/.contextstream/bin/contextstream-mcp");
        let hooks_path = Path::new("/home/alice/.codex/hooks.json");
        let win_override = codex_windows_hook_command(
            r"C:\Users\Alice\.contextstream\bin\contextstream-mcp.exe",
            CODEX_NOTICE_HOOK,
        );
        let shared_doc = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command",
                        "command": "\"/old/contextstream-mcp\" hook mid-run-notices --contextstream-managed-hook=v1",
                        "commandWindows": win_override,
                        "timeout": 5
                    }]
                }]
            }
        });

        let (updated, kind) = update_codex_hooks_document_for_platform(
            &shared_doc,
            hooks_path,
            managed.to_str().unwrap(),
            managed,
            false,
        )
        .expect("refresh on POSIX");
        assert_eq!(kind, CodexHookUpdateKind::MigratedExisting);
        assert_eq!(
            updated["hooks"]["PreToolUse"][0]["hooks"][0]["commandWindows"].as_str(),
            Some(win_override.as_str())
        );
        assert_eq!(
            validate_codex_notice_hooks_for_platform(&updated, managed, false),
            Ok(1)
        );
    }

    #[test]
    fn codex_trust_hash_and_config_toml_states_match_upstream_codex() {
        let temp = tempfile::tempdir().expect("tempdir");
        let hooks_path = temp.path().join("hooks.json");
        let config_path = temp.path().join("config.toml");
        let managed = Path::new(r"C:\Users\O'Brien\.contextstream\bin\contextstream-mcp.exe");

        let (doc, _) = update_codex_hooks_document_for_platform(
            &Value::Null,
            &hooks_path,
            managed.to_str().unwrap(),
            managed,
            true,
        )
        .expect("generate");
        std::fs::write(&hooks_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        // 1. Missing config.toml -> Untrusted.
        let untrusted = inspect_codex_hook_trust_for_platform(
            &hooks_path,
            &doc,
            Some(&config_path),
            managed,
            true,
        )
        .expect("untrusted");
        let current_hash = match untrusted {
            CodexHookTrustState::Untrusted { current_hash } => current_hash,
            other => panic!("expected Untrusted, got {other:?}"),
        };
        assert!(current_hash.starts_with("sha256:"));

        // On POSIX, adding/changing commandWindows does not alter the POSIX hash.
        let group = &doc["hooks"]["PreToolUse"][0];
        let handler = &group["hooks"][0];
        let mut without_win = handler.clone();
        without_win
            .as_object_mut()
            .unwrap()
            .remove("commandWindows");
        assert_eq!(
            codex_hook_identity_hash_for_platform(group, handler, false),
            codex_hook_identity_hash_for_platform(group, &without_win, false),
        );
        // On Windows, effective command switches from command to commandWindows, so hash changes!
        let old_win_hash =
            codex_hook_identity_hash_for_platform(group, &without_win, true).unwrap();
        assert_ne!(current_hash, old_win_hash);

        let state_key = format!("{}:pre_tool_use:0:0", hooks_path.display());

        // 2. Old trusted_hash -> Modified.
        let mut toml_doc = toml_edit::DocumentMut::new();
        toml_doc["hooks"]["state"][&state_key]["trusted_hash"] = toml_edit::value(&old_win_hash);
        std::fs::write(&config_path, toml_doc.to_string()).unwrap();
        assert_eq!(
            inspect_codex_hook_trust_for_platform(
                &hooks_path,
                &doc,
                Some(&config_path),
                managed,
                true,
            )
            .unwrap(),
            CodexHookTrustState::Modified {
                current_hash: current_hash.clone(),
                trusted_hash: old_win_hash,
            }
        );

        // 3. Matching trusted_hash -> Trusted.
        toml_doc["hooks"]["state"][&state_key]["trusted_hash"] = toml_edit::value(&current_hash);
        std::fs::write(&config_path, toml_doc.to_string()).unwrap();
        assert_eq!(
            inspect_codex_hook_trust_for_platform(
                &hooks_path,
                &doc,
                Some(&config_path),
                managed,
                true,
            )
            .unwrap(),
            CodexHookTrustState::Trusted {
                current_hash: current_hash.clone(),
            }
        );

        // 4. enabled = false -> DisabledByHookState.
        toml_doc["hooks"]["state"][&state_key]["enabled"] = toml_edit::value(false);
        std::fs::write(&config_path, toml_doc.to_string()).unwrap();
        assert_eq!(
            inspect_codex_hook_trust_for_platform(
                &hooks_path,
                &doc,
                Some(&config_path),
                managed,
                true,
            )
            .unwrap(),
            CodexHookTrustState::DisabledByHookState {
                current_hash: current_hash.clone(),
            }
        );

        // 5. [features] codex_hooks = false -> DisabledByFeatureFlag.
        toml_doc["features"]["codex_hooks"] = toml_edit::value(false);
        std::fs::write(&config_path, toml_doc.to_string()).unwrap();
        assert_eq!(
            inspect_codex_hook_trust_for_platform(
                &hooks_path,
                &doc,
                Some(&config_path),
                managed,
                true,
            )
            .unwrap(),
            CodexHookTrustState::DisabledByFeatureFlag { current_hash }
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_shell_probe_checks_exit_status_output_schema_and_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let input = sample_codex_pre_tool_use_input(temp.path(), DOCTOR_PROBE_SESSION_ID);

        // Valid empty and populated PreToolUse outputs pass.
        let empty = run_codex_hook_command_in_shell(
            CodexShellKind::UnixSh,
            "cat >/dev/null && printf '{}'",
            temp.path(),
            &input,
            Duration::from_secs(2),
            &[],
            true,
        )
        .expect("empty output");
        assert_eq!(empty.parsed_stdout, json!({}));

        let populated = run_codex_hook_command_in_shell(
            CodexShellKind::UnixSh,
            r#"cat >/dev/null && printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"[COORDINATION] hi"}}'"#,
            temp.path(),
            &input,
            Duration::from_secs(2),
            &[],
            true,
        )
        .expect("populated output");
        assert_eq!(
            populated.parsed_stdout["hookSpecificOutput"]["additionalContext"],
            "[COORDINATION] hi"
        );

        // Non-zero exit fails with stderr detail.
        let exit_err = run_codex_hook_command_in_shell(
            CodexShellKind::UnixSh,
            "echo \"Unexpected token 'hook'\" >&2; exit 1",
            temp.path(),
            &input,
            Duration::from_secs(2),
            &[],
            true,
        )
        .expect_err("non-zero exit must fail");
        assert!(exit_err.contains("exited with status 1"), "{exit_err}");
        assert!(exit_err.contains("Unexpected token 'hook'"), "{exit_err}");

        // Invalid Codex field fails schema check.
        let bad_json = run_codex_hook_command_in_shell(
            CodexShellKind::UnixSh,
            r#"cat >/dev/null && printf '{"user_message":"wrong-client"}'"#,
            temp.path(),
            &input,
            Duration::from_secs(2),
            &[],
            true,
        )
        .expect_err("unsupported field must fail");
        assert!(
            bad_json.contains("unsupported Codex PreToolUse field"),
            "{bad_json}"
        );

        // Hung command fails timeout check cleanly.
        let timeout_err = run_codex_hook_command_in_shell(
            CodexShellKind::UnixSh,
            "sleep 5",
            temp.path(),
            &input,
            Duration::from_millis(200),
            &[],
            true,
        )
        .expect_err("hung command must time out");
        assert!(timeout_err.contains("timed out"), "{timeout_err}");
    }
}
