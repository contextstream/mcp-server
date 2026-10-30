//! A rules file that merely names the managed block's tags in its own text (#366)
//! is the person's text: `update-rules` installs the block after it and
//! `uninstall` removes only the block. Runs the real binary through offline
//! commands.

use std::{
    path::Path,
    process::{Command, Output},
};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_sentence_naming_the_tags_does_not_stop_the_rules_install_and_survives_uninstall() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let codex = home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let agents = codex.join("AGENTS.md");
    let original = "# Notes\nWe document the <contextstream> tag here, and the closing </contextstream> too.\nKeep this line.\n";
    std::fs::write(&agents, original).unwrap();

    let installed = run(
        home,
        &["update-rules", "--scope", "global", "--editors", "codex"],
    );
    assert!(installed.status.success(), "{}", text(&installed));
    assert!(
        !text(&installed).contains("Refusing to modify"),
        "{}",
        text(&installed)
    );
    let written = std::fs::read_to_string(&agents).unwrap();
    assert!(
        written.starts_with(original),
        "the person's text stays first, untouched: {written:.200}"
    );
    assert!(
        written.len() > original.len() + 1000
            && written
                .lines()
                .filter(|line| *line == "<contextstream>")
                .count()
                == 1,
        "one managed block was added after it"
    );

    // Running it again changes nothing.
    let again = run(
        home,
        &["update-rules", "--scope", "global", "--editors", "codex"],
    );
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(std::fs::read_to_string(&agents).unwrap(), written);

    // Uninstall removes the block and leaves the sentence: byte for byte.
    let removed = run(
        home,
        &["uninstall", "--scope", "global", "--editors", "codex"],
    );
    assert!(removed.status.success(), "{}", text(&removed));
    assert_eq!(std::fs::read_to_string(&agents).unwrap(), original);
}
