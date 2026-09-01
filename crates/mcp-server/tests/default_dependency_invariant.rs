use std::process::Command;

#[test]
fn default_mcp_server_tree_has_no_mongodb_or_bson() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "mcp-server", "--no-default-features"])
        .output()
        .expect("cargo tree should run");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    for forbidden in ["mongodb", "bson"] {
        assert!(
            !tree.lines().any(|line| line.contains(forbidden)),
            "default mcp-server dependency tree must not include {forbidden}"
        );
    }
}
