use super::*;
use serde_json::json;

fn definition() -> Value {
    json!({
        "name": "check",
        "command_hints": {
            "argument": "command",
            "cwd_argument": "cwd",
            "patterns": ["^cargo (check|test)( .*)?$", "^npm (run )?test$"],
        }
    })
}

fn tool() -> CachedTool {
    CachedTool {
        name: "check".to_string(),
        transport: "stream".to_string(),
        attachments: vec!["checkout_snapshot".to_string()],
        hints: Hints::from_definition(&definition()).unwrap(),
    }
}

#[test]
fn hints_are_read_from_a_definition() {
    let hints = Hints::from_definition(&definition()).unwrap();
    assert_eq!(hints.argument, "command");
    assert_eq!(hints.cwd_argument.as_deref(), Some("cwd"));
    assert_eq!(hints.patterns.len(), 2);
    // A folder argument is optional.
    let mut bare = definition();
    bare["command_hints"]
        .as_object_mut()
        .unwrap()
        .remove("cwd_argument");
    assert_eq!(Hints::from_definition(&bare).unwrap().cwd_argument, None);
}

#[test]
fn hints_that_cannot_be_used_are_left_out() {
    for bad in [
        json!({"name": "x"}),
        json!({"command_hints": "nope"}),
        json!({"command_hints": {"patterns": ["^a"]}}),
        json!({"command_hints": {"argument": "has space", "patterns": ["^a"]}}),
        json!({"command_hints": {"argument": "command", "patterns": []}}),
        json!({"command_hints": {"argument": "command", "patterns": [7, ""]}}),
        // A pattern that does not compile, or has a look-around, is dropped; none left means no hints.
        json!({"command_hints": {"argument": "command", "patterns": ["(", "^a(?=b)"]}}),
    ] {
        assert!(Hints::from_definition(&bad).is_none(), "{bad}");
    }
    // The usable patterns stay when others are dropped; at most 64 are kept.
    let mixed = json!({"command_hints": {"argument": "c", "patterns": ["(", "^ok$"]}});
    assert_eq!(Hints::from_definition(&mixed).unwrap().patterns, ["^ok$"]);
    let many: Vec<String> = (0..100).map(|i| format!("^cmd{i}$")).collect();
    let many = json!({"command_hints": {"argument": "c", "patterns": many}});
    assert_eq!(Hints::from_definition(&many).unwrap().patterns.len(), 64);
    // A cwd argument that is not a plain name is ignored, not an error.
    let odd =
        json!({"command_hints": {"argument": "c", "cwd_argument": "a b", "patterns": ["^a"]}});
    assert_eq!(Hints::from_definition(&odd).unwrap().cwd_argument, None);
}

#[test]
fn hints_that_take_everyday_commands_are_not_used() {
    for broad in [
        ".*",
        "^.*$",
        "^(ls|cargo test)$",
        "git",
        "^git (status|diff)",
        "^[a-z]+( .*)?$",
        "echo",
    ] {
        let definition = json!({"command_hints": {"argument": "command", "patterns": [broad]}});
        assert!(Hints::from_definition(&definition).is_none(), "{broad}");
    }
    // One broad pattern spoils the whole set, since the set is what the tool takes.
    let mixed =
        json!({"command_hints": {"argument": "command", "patterns": ["^cargo test$", ".*"]}});
    assert!(Hints::from_definition(&mixed).is_none());
    // A kind of job is fine.
    let narrow = json!({"command_hints": {"argument": "command", "patterns": ["^cargo (check|test)( .*)?$", "^echo e2e-.*$"]}});
    assert!(Hints::from_definition(&narrow).is_some());
}

#[test]
fn a_command_matches_as_typed_and_trimmed_with_case_kept() {
    let hints = Hints::from_definition(&definition()).unwrap();
    for yes in [
        "cargo check",
        "cargo test -p thing --lib",
        "  cargo test  ",
        "npm test",
        "npm run test",
    ] {
        assert!(hints.matches(yes), "{yes:?}");
    }
    for no in [
        "",
        "   ",
        "Cargo check",
        "cargo build",
        "cargo checkout",
        "echo cargo test",
        "rm -rf / && cargo test",
        "npm run lint",
    ] {
        assert!(!hints.matches(no), "{no:?}");
    }
    // A command too long to be a command someone typed is never matched.
    assert!(!hints.matches(&format!("cargo test {}", "x".repeat(9000))));
}

#[test]
fn the_cache_is_kept_for_one_caller_and_one_api() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("sub").join("hints.json");
    let cache = Cache::new("local:aaaa", "https://api.example.com/", vec![tool()]);
    assert!(write_to(&path, &cache).unwrap());
    let read = read_from(&path, "local:aaaa", "https://API.example.com").unwrap();
    assert_eq!(read.tools, vec![tool()]);
    assert_eq!(read.tool_for("cargo check -p x").unwrap().name, "check");
    assert!(read.tool_for("make").is_none());
    assert_eq!(read.tool_named("check").unwrap().transport, "stream");
    assert!(read.tool_named("other").is_none());
    // Not another caller's, nor another API's.
    assert!(read_from(&path, "local:bbbb", "https://api.example.com").is_none());
    assert!(read_from(&path, "local:aaaa", "https://staging.example.com").is_none());
    // The credential is not in it.
    assert!(!std::fs::read_to_string(&path).unwrap().contains("secret"));
}

#[test]
fn writing_the_same_hints_again_does_not_write() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("hints.json");
    let cache = Cache::new("local:aaaa", "https://api.example.com", vec![tool()]);
    assert!(write_to(&path, &cache).unwrap());
    assert!(!write_to(&path, &cache).unwrap());
    // Other hints, or none, are written, so that a tool taken away stops being used.
    assert!(write_to(
        &path,
        &Cache::new("local:aaaa", "https://api.example.com", vec![])
    )
    .unwrap());
    assert!(read_from(&path, "local:aaaa", "https://api.example.com")
        .unwrap()
        .tools
        .is_empty());
}

#[test]
fn an_old_or_unreadable_cache_is_not_used() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("hints.json");
    let mut cache = Cache::new("local:aaaa", "https://api.example.com", vec![tool()]);
    cache.written_at = now_secs() - CACHE_MAX_AGE.as_secs() - 5;
    std::fs::write(&path, serde_json::to_string(&cache).unwrap()).unwrap();
    assert!(read_from(&path, "local:aaaa", "https://api.example.com").is_none());
    // Written again once it is stale, even when the hints are the same.
    let fresh = Cache::new("local:aaaa", "https://api.example.com", vec![tool()]);
    assert!(write_to(&path, &fresh).unwrap());
    assert!(read_from(&path, "local:aaaa", "https://api.example.com").is_some());
    std::fs::write(&path, "not json").unwrap();
    assert!(read_from(&path, "local:aaaa", "https://api.example.com").is_none());
    assert!(read_from(&folder.path().join("missing"), "x", "y").is_none());
    // A different version is not trusted.
    let mut other = Cache::new("local:aaaa", "https://api.example.com", vec![tool()]);
    other.version = 99;
    std::fs::write(&path, serde_json::to_string(&other).unwrap()).unwrap();
    assert!(read_from(&path, "local:aaaa", "https://api.example.com").is_none());
}

#[cfg(unix)]
#[test]
fn a_symlink_is_never_written_through_or_read() {
    let folder = tempfile::tempdir().unwrap();
    let target = folder.path().join("target.json");
    std::fs::write(&target, "keep me").unwrap();
    let link = folder.path().join("hints.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let cache = Cache::new("local:aaaa", "https://api.example.com", vec![tool()]);
    assert!(write_to(&link, &cache).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
    assert!(read_from(&link, "local:aaaa", "https://api.example.com").is_none());
}

#[test]
fn each_account_has_its_own_file() {
    let a = cache_path(&local_identity("one-key")).unwrap();
    let b = cache_path(&local_identity("another-key")).unwrap();
    assert_ne!(a, b);
    let name = a.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("hosted-command-hints-") && name.ends_with(".json"),
        "{name}"
    );
    assert!(!name.contains("one-key") && !name.contains('/') && !name.contains(".."));
    // Whatever an identity holds, the name stays one plain file name.
    let odd = cache_path("../../etc/passwd:x").unwrap();
    assert_eq!(odd.parent(), a.parent());
}

#[test]
fn the_identity_is_a_digest_and_the_switch_reads_like_the_others() {
    let identity = local_identity("a-secret-key");
    assert!(
        identity.starts_with("local:") && identity.len() == 22,
        "{identity}"
    );
    assert!(!identity.contains("secret"));
    assert_eq!(identity, local_identity("a-secret-key"));
    assert_ne!(identity, local_identity("another-key"));
    for off in ["off", "OFF", " 0 ", "false", "no", "disabled"] {
        assert!(switched_off(Some(off)), "{off}");
    }
    for on in ["on", "1", "", "yes"] {
        assert!(!switched_off(Some(on)), "{on}");
    }
    assert!(!switched_off(None));
}

#[test]
fn a_word_for_a_posix_shell_survives_quotes() {
    assert_eq!(shell_quote("cargo test"), "'cargo test'");
    assert_eq!(shell_quote("echo 'hi' && x"), "'echo '\\''hi'\\'' && x'");
    assert_eq!(shell_quote(""), "''");
}
