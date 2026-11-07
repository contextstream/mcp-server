//! Regression tests for Codex `hooks.json` generation, Windows `commandWindows`
//! escaping, 1.0.71 / workaround migration, live `doctor` shell execution
//! probing, and Codex `[hooks.state]` trust verification.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_server::setup::codex_hooks::{
    codex_hook_identity_hash_for_platform, codex_notice_hook_handler_for_platform,
    codex_windows_hook_command, effective_codex_hook_command_for_platform,
    parse_canonical_codex_windows_hook_command, run_codex_hook_command_in_shell,
    sample_codex_pre_tool_use_input, update_codex_hooks_document_for_platform,
    validate_codex_notice_hooks_for_platform, validate_codex_pre_tool_use_stdout,
    CodexHookUpdateKind, CodexShellKind,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TEST_SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
const TEST_NOTICE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest.as_slice() {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn managed_binary_path_in_home(home: &Path) -> PathBuf {
    let exe = if cfg!(windows) {
        "contextstream-mcp.exe"
    } else {
        "contextstream-mcp"
    };
    home.join(".contextstream").join("bin").join(exe)
}

fn install_helper_in_home(home: &Path) -> PathBuf {
    let dst = managed_binary_path_in_home(home);
    std::fs::create_dir_all(dst.parent().expect("bin parent")).expect("create bin dir");
    std::fs::copy(env!("CARGO_BIN_EXE_contextstream-mcp"), &dst).expect("copy helper binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dst).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dst, perms).expect("chmod helper");
    }
    dst
}

fn run_cli(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
        .env("NO_COLOR", "1")
        .output()
        .expect("execute contextstream-mcp")
}

fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, bytes));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn codex_windows_command_generator_round_trips_and_avoids_outer_shell_interpolation() {
    let tricky_paths = [
        (
            r"C:\Users\Jane Doe\.contextstream\bin\contextstream-mcp.exe",
            r"C:\Users\Jane Doe\.contextstream\bin\contextstream-mcp.exe",
        ),
        (
            r"C:\Users\O'Brien\Program Files (x86)\ContextStream & Co\contextstream-mcp.exe",
            r"C:\Users\O'Brien\Program Files (x86)\ContextStream & Co\contextstream-mcp.exe",
        ),
        (
            r"C:\Users\Dev$1%TEMP%`tick\O'Brien's Tools\contextstream-mcp.exe",
            r"C:\Users\Dev$1%TEMP%`tick\O'Brien's Tools\contextstream-mcp.exe",
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

    for (raw_input, expected_normalized) in tricky_paths {
        let cmd_win = codex_windows_hook_command(raw_input, "mid-run-notices");
        assert!(
            cmd_win.starts_with("powershell.exe -NoProfile -NonInteractive -Command \""),
            "unexpected wrapper prefix: {cmd_win}"
        );
        assert!(
            !cmd_win.contains('$') && !cmd_win.contains('`') && !cmd_win.contains('%'),
            "commandWindows must not contain PowerShell or cmd.exe expansion metacharacters: {cmd_win}"
        );
        assert_eq!(
            cmd_win.matches('"').count(),
            2,
            "commandWindows must contain only the outer -Command quotes: {cmd_win}"
        );
        let parsed = parse_canonical_codex_windows_hook_command(&cmd_win)
            .unwrap_or_else(|| panic!("failed to round-trip {cmd_win}"));
        assert_eq!(parsed.0, expected_normalized);
        assert_eq!(parsed.1, "mid-run-notices");

        let handler = codex_notice_hook_handler_for_platform(raw_input, true, None);
        let doc = json!({
            "hooks": {
                "PreToolUse": [{ "hooks": [handler] }]
            }
        });
        assert_eq!(
            validate_codex_notice_hooks_for_platform(&doc, Path::new(expected_normalized), true,),
            Ok(1),
            "generated Windows Codex hook must validate cleanly for {raw_input}"
        );
    }
}

#[test]
fn codex_updates_preserve_command_windows_and_migrate_1_0_71_and_workarounds() {
    let hooks_path = Path::new(r"C:\Users\O'Brien\.codex\hooks.json");
    let win_bin = r"C:\Users\O'Brien\Program Files\ContextStream\contextstream-mcp.exe";
    let managed_bin = Path::new(win_bin);
    let user_audit = json!({
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "python C:\\tools\\audit.py" }]
    });
    let user_post = json!([{
        "hooks": [{ "type": "command", "command": "python C:\\tools\\post.py" }]
    }]);

    let legacy_variants = [
        // 1. Version 1.0.71 broken bare double-quoted executable path (no commandWindows).
        json!({
            "type": "command",
            "command": format!("\"{win_bin}\" hook mid-run-notices --contextstream-managed-hook=v1"),
            "timeout": 5
        }),
        // 2. Local unquoted-path workaround in commandWindows.
        json!({
            "type": "command",
            "command": format!("\"{win_bin}\" hook mid-run-notices --contextstream-managed-hook=v1"),
            "commandWindows": r"C:\Users\alice\.contextstream\bin\contextstream-mcp.exe hook mid-run-notices --contextstream-managed-hook=v1",
            "timeout": 5
        }),
        // 3a. Local PowerShell call-operator workaround with doubled apostrophes.
        json!({
            "type": "command",
            "command": format!("& '{}' hook mid-run-notices", win_bin.replace('\'', "''")),
            "timeout": 5
        }),
        // 3b. Hand-edited PowerShell call-operator workaround with unescaped apostrophe.
        json!({
            "type": "command",
            "command": format!("& '{win_bin}' hook mid-run-notices"),
            "timeout": 5
        }),
        // 4. Snake-case command_windows alias with custom wrapper.
        json!({
            "type": "command",
            "command": format!("\"{win_bin}\" hook mid-run-notices --contextstream-managed-hook=v1"),
            "command_windows": format!("powershell -Command \"& '{win_bin}' hook mid-run-notices --contextstream-managed-hook=v1\""),
            "timeout": 5
        }),
    ];

    for variant in legacy_variants {
        let initial = json!({
            "hooks": {
                "PreToolUse": [
                    user_audit.clone(),
                    { "hooks": [variant.clone()] }
                ],
                "PostToolUse": user_post.clone()
            }
        });
        assert!(
            validate_codex_notice_hooks_for_platform(&initial, managed_bin, true).is_err(),
            "legacy/workaround variant must be flagged for migration: {variant}"
        );

        let (migrated, kind) = update_codex_hooks_document_for_platform(
            &initial,
            hooks_path,
            win_bin,
            managed_bin,
            true,
        )
        .expect("migrate Codex hooks");
        assert_eq!(kind, CodexHookUpdateKind::MigratedExisting);
        assert_eq!(
            validate_codex_notice_hooks_for_platform(&migrated, managed_bin, true),
            Ok(1)
        );
        assert_eq!(migrated["hooks"]["PreToolUse"][0], user_audit);
        assert_eq!(migrated["hooks"]["PostToolUse"], user_post);

        let installed_handler = &migrated["hooks"]["PreToolUse"][1]["hooks"][0];
        assert_eq!(
            installed_handler["commandWindows"].as_str(),
            Some(codex_windows_hook_command(win_bin, "mid-run-notices").as_str())
        );

        // Subsequent updates on Windows keep commandWindows intact and report Unchanged.
        let (rerun, rerun_kind) = update_codex_hooks_document_for_platform(
            &migrated,
            hooks_path,
            win_bin,
            managed_bin,
            true,
        )
        .expect("idempotent rerun");
        assert_eq!(rerun_kind, CodexHookUpdateKind::Unchanged);
        assert_eq!(rerun, migrated);

        // Even a POSIX refresh on a shared ~/.codex/hooks.json preserves canonical commandWindows.
        let posix_bin = "/home/obrien/.contextstream/bin/contextstream-mcp";
        let (posix_refreshed, _) = update_codex_hooks_document_for_platform(
            &migrated,
            Path::new("/home/obrien/.codex/hooks.json"),
            posix_bin,
            Path::new(posix_bin),
            false,
        )
        .expect("POSIX refresh on shared dotfiles");
        assert_eq!(
            posix_refreshed["hooks"]["PreToolUse"][1]["hooks"][0]["commandWindows"],
            installed_handler["commandWindows"],
            "POSIX update-hooks must not strip an existing canonical commandWindows override"
        );
    }
}

#[test]
fn real_binary_in_space_and_apostrophe_path_forwards_stdin_and_emits_empty_and_populated_notices() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workdir = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workdir).expect("workdir");

    let exe_name = if cfg!(windows) {
        "contextstream-mcp.exe"
    } else {
        "contextstream-mcp"
    };
    let tricky_dir = temp
        .path()
        .join("Program Files (x86)")
        .join("O'Brien's Tools");
    std::fs::create_dir_all(&tricky_dir).expect("tricky dir");
    let tricky_bin = tricky_dir.join(exe_name);
    std::fs::copy(env!("CARGO_BIN_EXE_contextstream-mcp"), &tricky_bin).expect("copy binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tricky_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tricky_bin, perms).unwrap();
    }

    let handler =
        codex_notice_hook_handler_for_platform(&tricky_bin.to_string_lossy(), cfg!(windows), None);
    let effective = effective_codex_hook_command_for_platform(&handler, cfg!(windows))
        .expect("effective command")
        .to_string();

    let mut shells: Vec<(CodexShellKind, String)> = Vec::new();
    #[cfg(unix)]
    {
        shells.push((CodexShellKind::UnixSh, effective.clone()));
    }
    #[cfg(windows)]
    {
        shells.push((CodexShellKind::PowerShell, effective.clone()));
        shells.push((CodexShellKind::CmdExe, effective.clone()));
    }
    if which::which("pwsh").is_ok() {
        let win_cmd = codex_windows_hook_command(&tricky_bin.to_string_lossy(), "mid-run-notices");
        // On Windows `powershell.exe` is inside `win_cmd`; on Unix with pwsh installed we can
        // also test the inner PowerShell expression directly if powershell.exe isn't present.
        if cfg!(windows) {
            shells.push((CodexShellKind::Pwsh, win_cmd));
        }
    }

    let input = sample_codex_pre_tool_use_input(&workdir, TEST_SESSION_ID);
    let session_key = sha256_hex(format!("session:{TEST_SESSION_ID}").as_bytes());
    let notices_dir = home.join(".contextstream").join("mid-run-notices");
    std::fs::create_dir_all(&notices_dir).expect("notices dir");
    let state_file = notices_dir.join(format!("{session_key}.json"));

    let env_pairs: Vec<(&str, &OsStr)> = vec![
        ("HOME", home.as_os_str()),
        ("USERPROFILE", home.as_os_str()),
        ("CONTEXTSTREAM_WATCH", OsStr::new("0")),
        ("CONTEXTSTREAM_WATCH_DISABLED", OsStr::new("1")),
    ];

    for (shell, command_str) in &shells {
        // 1. Empty notice state -> `{}` on stdout, exit 0.
        std::fs::write(
            &state_file,
            serde_json::to_vec(&json!({
                "last_poll_unix_ms": now_unix_ms(),
                "checked_in_summary": true,
                "baselined_static": true,
                "pending": []
            }))
            .unwrap(),
        )
        .unwrap();

        let empty_probe = run_codex_hook_command_in_shell(
            *shell,
            command_str,
            &workdir,
            &input,
            Duration::from_secs(10),
            &env_pairs,
            false,
        )
        .unwrap_or_else(|err| panic!("empty notice probe failed in {:?}: {err}", shell));
        assert_eq!(empty_probe.parsed_stdout, json!({}), "shell={shell:?}");

        // 2. Populated notice state keyed by stdin's `session_id` -> `additionalContext`,
        // proving both JSON stdin forwarding and Codex PreToolUse output compliance.
        let expected_notice = format!(
            "[CONTEXTSTREAM LIVE NOTICE] New while you were working:\n[COORDINATION] (info) notice_id=\"{TEST_NOTICE_ID}\" Verify Windows Codex hook execution."
        );
        std::fs::write(
            &state_file,
            serde_json::to_vec(&json!({
                "last_poll_unix_ms": now_unix_ms(),
                "checked_in_summary": true,
                "baselined_static": true,
                "pending": [expected_notice]
            }))
            .unwrap(),
        )
        .unwrap();

        let populated_probe = run_codex_hook_command_in_shell(
            *shell,
            command_str,
            &workdir,
            &input,
            Duration::from_secs(10),
            &env_pairs,
            false,
        )
        .unwrap_or_else(|err| panic!("populated notice probe failed in {:?}: {err}", shell));
        let validated = validate_codex_pre_tool_use_stdout(&populated_probe.raw_stdout)
            .expect("valid Codex PreToolUse stdout");
        assert_eq!(
            validated["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
        let additional = validated["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext string");
        assert!(
            additional.contains(TEST_NOTICE_ID),
            "stdin session_id must route to seeded notice in {shell:?}, got: {additional}"
        );

        // 3. Immediately following call drains back to `{}`.
        let drained_probe = run_codex_hook_command_in_shell(
            *shell,
            command_str,
            &workdir,
            &input,
            Duration::from_secs(10),
            &env_pairs,
            false,
        )
        .unwrap_or_else(|err| panic!("drained probe failed in {:?}: {err}", shell));
        assert_eq!(drained_probe.parsed_stdout, json!({}), "shell={shell:?}");
    }

    // 4. On Windows, explicitly demonstrate that the v1.0.71 bare double-quoted
    // path fails in Windows PowerShell with `Unexpected token 'hook'`, while our
    // `commandWindows` override above succeeded in both PowerShell and cmd.exe.
    #[cfg(windows)]
    {
        let broken_1_0_71 = format!(
            "\"{}\" hook mid-run-notices --contextstream-managed-hook=v1",
            tricky_bin.display()
        );
        let err = run_codex_hook_command_in_shell(
            CodexShellKind::PowerShell,
            &broken_1_0_71,
            &workdir,
            &input,
            Duration::from_secs(10),
            &env_pairs,
            true,
        )
        .expect_err("bare double-quoted executable path must fail in PowerShell");
        assert!(
            err.contains("hook"),
            "expected PowerShell Unexpected token 'hook' error, got: {err}"
        );
    }
}

#[test]
fn cli_doctor_executes_codex_hook_migrates_broken_install_and_checks_trust_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workdir = temp.path().join("project");
    std::fs::create_dir_all(home.join(".codex")).expect("codex dir");
    std::fs::create_dir_all(&workdir).expect("workdir");

    let managed_bin = install_helper_in_home(&home);
    std::fs::write(
        home.join(".contextstream").join("credentials.json"),
        serde_json::to_string_pretty(&json!({
            "api_key": "existing-test-key",
            "api_url": "http://127.0.0.1:9"
        }))
        .unwrap(),
    )
    .expect("seed credentials.json");
    let hooks_path = home.join(".codex").join("hooks.json");
    let config_toml = home.join(".codex").join("config.toml");

    // Seed a broken ContextStream Codex hook command that passes naive substring checks
    // ("--contextstream-managed-hook=v1" + "mid-run-notices") alongside a user hook,
    // but points to a nonexistent executable so shell execution fails.
    let missing_bin = temp
        .path()
        .join("Program Files")
        .join("O'Brien's Missing")
        .join(if cfg!(windows) {
            "contextstream-mcp.exe"
        } else {
            "contextstream-mcp"
        });
    let broken_doc = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "echo user-audit" }]
                },
                {
                    "hooks": [{
                        "type": "command",
                        "command": format!("\"{}\" hook mid-run-notices --contextstream-managed-hook=v1", missing_bin.display()),
                        "timeout": 5
                    }]
                }
            ]
        }
    });
    std::fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&broken_doc).unwrap(),
    )
    .unwrap();

    // 1. `doctor --json` must execute the hook, report `fail`, and write nothing to HOME.
    let before_doctor_tree = snapshot_tree(&home);
    let broken_doctor = run_cli(
        &home,
        &workdir,
        &[
            "doctor",
            "--json",
            "--scope",
            "global",
            "--editors",
            "codex",
        ],
    );
    assert!(
        !broken_doctor.status.success(),
        "doctor must exit non-zero when Codex hook execution fails:\n{}",
        String::from_utf8_lossy(&broken_doctor.stdout)
    );
    assert_eq!(
        snapshot_tree(&home),
        before_doctor_tree,
        "doctor without --repair must not create or modify any files under HOME"
    );
    let broken_report: Value =
        serde_json::from_slice(&broken_doctor.stdout).expect("parse doctor JSON");
    let hooks_check = broken_report["editors"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["surface"] == "hooks")
        .expect("hooks surface check");
    assert_eq!(hooks_check["status"], "fail", "{hooks_check:#}");

    // 2. `doctor --repair --dry-run` previews `.codex/hooks.json` migration + `/hooks` guidance
    // without mutating disk.
    let dry_run = run_cli(
        &home,
        &workdir,
        &[
            "doctor",
            "--repair",
            "--dry-run",
            "--scope",
            "global",
            "--editors",
            "codex",
        ],
    );
    let dry_stdout = String::from_utf8_lossy(&dry_run.stdout);
    let dry_stderr = String::from_utf8_lossy(&dry_run.stderr);
    assert!(
        dry_stdout.contains("hooks.json") && dry_stdout.contains("/hooks"),
        "expected dry-run repair output to mention hooks.json and /hooks:\nstdout:\n{dry_stdout}\nstderr:\n{dry_stderr}"
    );
    assert_eq!(
        snapshot_tree(&home),
        before_doctor_tree,
        "doctor --repair --dry-run must not mutate HOME"
    );

    // 3. `update-hooks --scope global --editors codex` migrates the broken hook to the
    // canonical generator, preserves the user's hook, and prints `/hooks` + reload guidance.
    let update = run_cli(
        &home,
        &workdir,
        &["update-hooks", "--scope", "global", "--editors", "codex"],
    );
    assert!(
        update.status.success(),
        "update-hooks failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&update.stdout),
        String::from_utf8_lossy(&update.stderr)
    );
    let update_stderr = String::from_utf8_lossy(&update.stderr);
    assert!(
        update_stderr.contains("/hooks") && update_stderr.contains("Codex"),
        "update-hooks should explain Codex /hooks review or reload:\n{update_stderr}"
    );

    let migrated_doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    assert_eq!(
        validate_codex_notice_hooks_for_platform(&migrated_doc, &managed_bin, cfg!(windows)),
        Ok(1)
    );
    assert_eq!(
        migrated_doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "echo user-audit",
        "unrelated user hook must be preserved"
    );
    if cfg!(windows) {
        assert!(
            migrated_doc["hooks"]["PreToolUse"][1]["hooks"][0]
                .get("commandWindows")
                .and_then(Value::as_str)
                .is_some(),
            "Windows install must emit commandWindows: {migrated_doc:#}"
        );
    }

    // 4. `doctor --json` now executes the real hook binary cleanly (writing zero files under
    // HOME) and reports `warn` with `fix: null` until Codex trusts the hook hash.
    let before_untrusted_doctor = snapshot_tree(&home);
    let untrusted_doctor = run_cli(
        &home,
        &workdir,
        &[
            "doctor",
            "--json",
            "--scope",
            "global",
            "--editors",
            "codex",
        ],
    );
    assert_eq!(
        snapshot_tree(&home),
        before_untrusted_doctor,
        "live doctor hook probe must not create mid-run-notices state files under HOME"
    );
    let untrusted_report: Value =
        serde_json::from_slice(&untrusted_doctor.stdout).expect("parse untrusted doctor JSON");
    let untrusted_hooks = untrusted_report["editors"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["surface"] == "hooks")
        .expect("hooks surface check");
    assert_eq!(untrusted_hooks["status"], "warn", "{untrusted_hooks:#}");
    assert!(untrusted_hooks["fix"].is_null(), "{untrusted_hooks:#}");
    assert!(
        untrusted_hooks["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("/hooks"),
        "{untrusted_hooks:#}"
    );

    // 5. Simulate user approving `/hooks` in Codex CLI by recording `trusted_hash` in
    // `~/.codex/config.toml` -> `doctor --json` upgrades `hooks` to `pass`!
    let group = &migrated_doc["hooks"]["PreToolUse"][1];
    let handler = &group["hooks"][0];
    let current_hash = codex_hook_identity_hash_for_platform(group, handler, cfg!(windows))
        .expect("codex hook identity hash");
    let state_key = format!("{}:pre_tool_use:1:0", hooks_path.display());
    std::fs::write(
        &config_toml,
        format!(
            "[features]\ncodex_hooks = true\n\n[hooks.state.{state_key:?}]\nenabled = true\ntrusted_hash = {current_hash:?}\n"
        ),
    )
    .unwrap();

    let trusted_doctor = run_cli(
        &home,
        &workdir,
        &[
            "doctor",
            "--json",
            "--scope",
            "global",
            "--editors",
            "codex",
        ],
    );
    let trusted_report: Value =
        serde_json::from_slice(&trusted_doctor.stdout).expect("parse trusted doctor JSON");
    let trusted_hooks = trusted_report["editors"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["surface"] == "hooks")
        .expect("hooks surface check");
    assert_eq!(trusted_hooks["status"], "pass", "{trusted_hooks:#}");
    assert!(
        trusted_hooks["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("trusted in Codex"),
        "{trusted_hooks:#}"
    );
}
