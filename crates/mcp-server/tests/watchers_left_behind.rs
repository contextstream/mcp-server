//! Fails when a CLI test left a `contextstream-mcp watch` running.
//!
//! A CLI test turns the sync bridge off with `CONTEXTSTREAM_WATCH=0`, the
//! one opt-out, or stops the watcher it started. A watcher left behind keeps
//! running long after its test and its temporary home are gone. Cargo runs
//! the test targets by name, so this one runs after the others and sees what
//! they left.
#![cfg(target_os = "linux")]

#[path = "support/watchers.rs"]
mod watchers;

#[test]
fn no_cli_test_left_a_watcher_running() {
    // This run began when cargo, this test's parent, started. A watcher that
    // started since then and runs this very build was started by a test of
    // this run; one from another checkout or an earlier run is not.
    let began = watchers::parent(std::process::id())
        .and_then(watchers::started)
        .unwrap_or(0);
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    let left: Vec<_> = watchers::watchers()
        .into_iter()
        .filter(|watcher| watcher.started >= began && watchers::runs(watcher.pid, binary))
        .collect();
    // A watcher whose home is gone serves no one: stop it, so the next run
    // starts clean.
    for watcher in &left {
        if watcher.home.as_deref().is_none_or(|home| !home.exists()) {
            watchers::stop(watcher.pid);
        }
    }
    assert!(
        left.is_empty(),
        "CLI tests left these `contextstream-mcp watch` processes running \
         (those whose home is gone were stopped): {left:#?}. A test that runs \
         the CLI sets CONTEXTSTREAM_WATCH=0, or stops the watcher it started."
    );
}
