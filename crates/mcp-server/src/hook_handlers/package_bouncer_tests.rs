//! Tests for the Package Bouncer hook.

use super::*;
use std::collections::HashMap;

fn env() -> Environment {
    Environment {
        home: None,
        vars: Vec::new(),
        system_pip_conf: None,
    }
}

fn names(command: &str) -> Vec<String> {
    requested_packages(command, None, &env())
        .into_iter()
        .map(|p| {
            let tag = match p.ecosystem {
                Ecosystem::Npm => "npm",
                Ecosystem::PyPi => "pypi",
                Ecosystem::Crates => "crates",
            };
            format!("{tag}:{}", p.name)
        })
        .collect()
}

#[test]
fn shell_words_keep_quotes_operators_and_redirections_apart() {
    assert_eq!(
        shell_words(r#"cd web && npm i "left pad" 'b c' d\ e 2>&1 | tee log; echo ok"#),
        vec![
            "cd", "web", "&&", "npm", "i", "left pad", "b c", "d e", "2>&1", "|", "tee", "log",
            ";", "echo", "ok"
        ]
    );
}

#[test]
fn install_commands_name_their_packages() {
    for (command, want) in [
        (
            "npm install express@4 @types/node@^20 lodash",
            vec!["npm:express", "npm:@types/node", "npm:lodash"],
        ),
        ("npm i -D --save-exact vitest", vec!["npm:vitest"]),
        ("npm install -w packages/a lodash", vec!["npm:lodash"]),
        (
            "pnpm add -F web zod && yarn add react",
            vec!["npm:zod", "npm:react"],
        ),
        ("bun add hono", vec!["npm:hono"]),
        (
            "npx -y create-vite my-app --template react",
            vec!["npm:create-vite"],
        ),
        ("npx -p @scope/tool tool --help", vec!["npm:@scope/tool"]),
        ("npm exec --package=cowsay -- cowsay hi", vec!["npm:cowsay"]),
        (
            "claude mcp add docs -- npx -y docs-mcp-server",
            vec!["npm:docs-mcp-server"],
        ),
        (
            "sudo -E FOO=1 npm install -g typescript",
            vec!["npm:typescript"],
        ),
        ("npm i real-alias@npm:actual-pkg@1", vec!["npm:actual-pkg"]),
        (
            "pip install 'requests[socks]>=2' Django_REST.framework==3",
            vec!["pypi:requests", "pypi:django-rest-framework"],
        ),
        (
            "python3 -m pip install -U --upgrade-strategy eager numpy",
            vec!["pypi:numpy"],
        ),
        (
            "uv add fastapi --group dev pytest",
            vec!["pypi:fastapi", "pypi:pytest"],
        ),
        ("uv pip install httpx", vec!["pypi:httpx"]),
        ("poetry add -G dev ruff", vec!["pypi:ruff"]),
        ("uvx ruff check .", vec!["pypi:ruff"]),
        (
            "uvx --from httpie http GET example.com",
            vec!["pypi:httpie"],
        ),
        ("pipx install black", vec!["pypi:black"]),
        (
            "cargo add serde --features derive tokio@1",
            vec!["crates:serde", "crates:tokio"],
        ),
        ("cargo install --locked ripgrep", vec!["crates:ripgrep"]),
        ("npm install foo > install.log 2>&1", vec!["npm:foo"]),
    ] {
        assert_eq!(names(command), want, "{command}");
    }
}

#[test]
fn commands_that_install_nothing_from_a_public_registry_name_nothing() {
    for command in [
        "npm install",
        "npm ci",
        "npm run build",
        "echo npm install made-up-thing",
        "git commit -m 'npm install foo'",
        "npm install user/repo ./local ../other.tgz https://x.dev/p.tgz git+ssh://git@github.com/a/b",
        "npm install --registry https://npm.internal lodash",
        "pip install -r requirements.txt -e .",
        "pip install -i https://pypi.internal/simple private-pkg",
        "pip install --extra-index-url=https://pypi.internal/simple private-pkg",
        "uv add --index https://pypi.internal/simple private-pkg",
        "cargo add --git https://github.com/a/b crate-x",
        "cargo add --registry internal crate-x",
        "cargo build",
        "pip list",
    ] {
        assert_eq!(names(command), Vec::<String>::new(), "{command}");
    }
}

#[test]
fn a_project_on_a_private_registry_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        "@acme:registry=https://npm.acme.dev/\n",
    )
    .unwrap();
    let nested = dir.path().join("app");
    std::fs::create_dir(&nested).unwrap();
    assert!(requested_packages("npm i @acme/ui left-pad", Some(&nested), &env()).is_empty());
    // A public registry line is not a private one.
    std::fs::write(
        dir.path().join(".npmrc"),
        "registry=https://registry.npmjs.org/\n",
    )
    .unwrap();
    assert_eq!(
        requested_packages("npm i left-pad", Some(&nested), &env()).len(),
        1
    );

    std::fs::write(
        dir.path().join("pip.conf"),
        "[global]\nindex-url = https://pypi.acme.dev/simple\n",
    )
    .unwrap();
    assert!(requested_packages("pip install acme-tools", Some(&nested), &env()).is_empty());
    // npm and crates are judged still.
    assert_eq!(
        requested_packages("cargo add serde", Some(&nested), &env()).len(),
        1
    );

    let vars = Environment {
        vars: vec![(
            "UV_INDEX_URL".to_string(),
            "https://pypi.acme.dev".to_string(),
        )],
        ..env()
    };
    assert!(requested_packages("uv add acme-tools", None, &vars).is_empty());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".npmrc"),
        "registry = https://npm.acme.dev\n",
    )
    .unwrap();
    let home_env = Environment {
        home: Some(home.path().to_path_buf()),
        ..env()
    };
    assert!(requested_packages("npm i x", None, &home_env).is_empty());
}

#[test]
fn names_are_read_as_their_registry_spells_them() {
    assert_eq!(npm_name("@types/node@^20").as_deref(), Some("@types/node"));
    assert_eq!(npm_name("lodash@4.17.21").as_deref(), Some("lodash"));
    assert_eq!(npm_name("user/repo"), None);
    assert_eq!(npm_name("@scope"), None);
    assert_eq!(
        npm_name("JSONStream"),
        None,
        "a legacy capitalized name is left alone"
    );
    assert_eq!(pip_name("Pillow>=10").as_deref(), Some("pillow"));
    assert_eq!(
        pip_name("zope.interface").as_deref(),
        Some("zope-interface")
    );
    assert_eq!(pip_name("pkg @ https://x.dev/pkg.whl"), None);
}

#[test]
fn a_lookalike_is_one_edit_or_separator_away_from_a_popular_name() {
    let pypi = |name: &str| Requested {
        ecosystem: Ecosystem::PyPi,
        name: name.to_string(),
    };
    let npm = |name: &str| Requested {
        ecosystem: Ecosystem::Npm,
        name: name.to_string(),
    };
    assert_eq!(lookalike_of(&pypi("reqeusts")), Some("requests"));
    assert_eq!(lookalike_of(&pypi("requets")), Some("requests"));
    assert_eq!(lookalike_of(&pypi("requestss")), Some("requests"));
    assert_eq!(lookalike_of(&pypi("requests")), None);
    assert_eq!(lookalike_of(&npm("lodahs")), Some("lodash"));
    assert_eq!(lookalike_of(&npm("react_dom")), Some("react-dom"));
    assert_eq!(lookalike_of(&npm("left-pad")), None);
    // Short names are too close to everything to judge by one edit.
    assert_eq!(lookalike_of(&npm("vuex")), None);
}

struct FakeRegistry {
    answers: HashMap<&'static str, Lookup>,
    delay: Duration,
}

#[async_trait]
impl Registry for FakeRegistry {
    async fn lookup(&self, package: &Requested) -> Lookup {
        tokio::time::sleep(self.delay).await;
        self.answers
            .get(package.name.as_str())
            .cloned()
            .unwrap_or(Lookup::Unknown)
    }
}

fn fake(answers: Vec<(&'static str, Lookup)>) -> FakeRegistry {
    FakeRegistry {
        answers: answers.into_iter().collect(),
        delay: Duration::ZERO,
    }
}

fn pypi(name: &str) -> Requested {
    Requested {
        ecosystem: Ecosystem::PyPi,
        name: name.to_string(),
    }
}

#[tokio::test]
async fn a_package_the_registry_lacks_is_denied_with_the_name_it_imitates() {
    let registry = fake(vec![
        ("reqeusts", Lookup::Missing),
        (
            "httpx",
            Lookup::Found {
                weekly_downloads: Some(9_000_000),
            },
        ),
    ]);
    let verdict = check(
        &[pypi("httpx"), pypi("reqeusts")],
        &registry,
        Duration::from_secs(1),
    )
    .await;
    let Verdict::Deny(message) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(
        message.contains("`reqeusts` is not on PyPI (did you mean `requests`?)"),
        "{message}"
    );
    assert!(
        message.contains("CONTEXTSTREAM_PACKAGE_BOUNCER=off"),
        "{message}"
    );
}

#[tokio::test]
async fn a_barely_used_lookalike_is_asked_about_and_a_popular_package_passes() {
    let registry = fake(vec![
        (
            "requestss",
            Lookup::Found {
                weekly_downloads: Some(12),
            },
        ),
        (
            "left-pad",
            Lookup::Found {
                weekly_downloads: Some(5),
            },
        ),
        (
            "requests",
            Lookup::Found {
                weekly_downloads: Some(80_000_000),
            },
        ),
    ]);
    let ask = check(&[pypi("requestss")], &registry, Duration::from_secs(1)).await;
    let Verdict::Ask(message) = ask else {
        panic!("{ask:?}")
    };
    assert!(message.contains("`requestss` on PyPI is a name away from the popular `requests` and had 12 downloads last week"), "{message}");
    // Few downloads alone, or a popular package, is no reason to stop.
    assert_eq!(
        check(
            &[pypi("left-pad"), pypi("requests")],
            &registry,
            Duration::from_secs(1)
        )
        .await,
        Verdict::Allow
    );
}

#[tokio::test]
async fn a_lookup_that_fails_or_runs_long_never_blocks() {
    assert_eq!(
        check(&[pypi("unknown")], &fake(vec![]), Duration::from_secs(1)).await,
        Verdict::Allow
    );
    assert_eq!(
        check(&[], &fake(vec![]), Duration::from_secs(1)).await,
        Verdict::Allow
    );
    let slow = FakeRegistry {
        answers: [("reqeusts", Lookup::Missing)].into_iter().collect(),
        delay: Duration::from_secs(5),
    };
    assert_eq!(
        check(&[pypi("reqeusts")], &slow, Duration::from_millis(50)).await,
        Verdict::Allow
    );
}

#[test]
fn only_a_shell_command_is_checked() {
    let claude = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "npm i x"}});
    assert_eq!(shell_command(&claude).as_deref(), Some("npm i x"));
    let edit = serde_json::json!({"tool_name": "Edit", "tool_input": {"command": "npm i x"}});
    assert_eq!(shell_command(&edit), None);
    let cursor =
        serde_json::json!({"hook_event_name": "beforeShellExecution", "command": "pip install y"});
    assert_eq!(shell_command(&cursor).as_deref(), Some("pip install y"));
}
