//! Account settings and project markers for the hosted `check` tool.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::RequestOptions;
use crate::ContextStreamClient;
use mcp_types::Result;

const ENABLED: &str = "/mcp/hosted-tools/enabled";
const MORE_ENGINES: &str = "/mcp/hosted-tools/more-engines";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Language {
    pub id: String,
    pub name: String,
    pub tools: String,
    pub more: bool,
    pub markers: Vec<String>,
}

fn builtin_languages() -> Vec<Language> {
    serde_json::from_str(include_str!("remote_check_languages.json"))
        .expect("built-in engine catalog")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pricing {
    pub credits_per_10s: u64,
    pub unit: String,
    pub billing: String,
    pub free: Vec<String>,
}

fn default_pricing() -> Option<Pricing> {
    Some(Pricing {
        credits_per_10s: 20,
        unit: "Context Credits".into(),
        billing: "run_time".into(),
        free: [
            "Exact repeats",
            "Checks that run locally",
            "Checks that never start",
        ]
        .map(String::from)
        .into(),
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credits {
    pub available: Option<i64>,
    pub no_charge: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteChecks {
    pub tool: String,
    pub enabled: bool,
    pub available: bool,
    /// True when the account is explicitly enabled on the server's policy row
    /// (`account_ids`), so self-service `off` cannot disable it account-wide.
    #[serde(default)]
    pub policy_locked: bool,
    pub more_engines: Option<bool>,
    /// Omitted by older APIs (use the published price); explicitly null for free checks.
    #[serde(default = "default_pricing")]
    pub pricing: Option<Pricing>,
    pub credits: Option<Credits>,
    #[serde(default = "builtin_languages")]
    pub languages: Vec<Language>,
}

#[derive(Deserialize)]
struct MoreEngines {
    enabled: bool,
}

// Setup and doctor must stay responsive even when this optional feature is unreachable.
fn options() -> Option<RequestOptions> {
    Some(RequestOptions {
        timeout: Some(Duration::from_secs(3)),
        retries: Some(0),
        ..Default::default()
    })
}

impl ContextStreamClient {
    pub async fn remote_checks(&self) -> Result<RemoteChecks> {
        self.request("GET", ENABLED, None::<()>, options()).await
    }

    pub async fn set_remote_checks(
        &self,
        enabled: bool,
        more_engines: Option<bool>,
    ) -> Result<RemoteChecks> {
        let mut body = json!({ "enabled": enabled });
        if let Some(more) = more_engines {
            body["more_engines"] = json!(more);
        }
        self.request("PUT", ENABLED, Some(body), options()).await
    }

    pub async fn more_engines(&self) -> Result<bool> {
        let answer: MoreEngines = self
            .request("GET", MORE_ENGINES, None::<()>, options())
            .await?;
        Ok(answer.enabled)
    }

    pub async fn set_more_engines(&self, enabled: bool) -> Result<bool> {
        let answer: MoreEngines = self
            .request(
                "PUT",
                MORE_ENGINES,
                Some(json!({ "enabled": enabled })),
                options(),
            )
            .await?;
        Ok(answer.enabled)
    }

    pub async fn remote_checks_status(&self) -> Result<RemoteChecks> {
        let mut status = self.remote_checks().await?;
        if status.more_engines.is_none() {
            status.more_engines = self.more_engines().await.ok();
        }
        Ok(status)
    }
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", "vendor", "dist", "build"];

/// Only regular filenames are needed. Never follow a symlink into another project.
pub fn project_filenames(root: &Path) -> std::io::Result<HashSet<String>> {
    let mut names = HashSet::new();
    for entry in root.read_dir()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_file() {
            names.insert(name);
        } else if kind.is_dir() && !SKIP_DIRS.contains(&name.as_str()) {
            if let Ok(children) = entry.path().read_dir() {
                for child in children.flatten() {
                    if child.file_type().is_ok_and(|kind| kind.is_file()) {
                        names.insert(child.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    Ok(names)
}

/// Apply the same depth and directory rules to indexed paths from a hosted session.
pub fn indexed_filename(path: &str) -> Option<&str> {
    let mut parts = path.strip_prefix("./").unwrap_or(path).split('/');
    let first = parts.next()?;
    match (parts.next(), parts.next()) {
        (None, _) => Some(first),
        (Some(name), None) if !SKIP_DIRS.contains(&first) => Some(name),
        _ => None,
    }
}

fn matches_marker(names: &HashSet<String>, marker: &str) -> bool {
    if !marker.contains(['*', '?', '[', '{']) {
        return names.contains(marker);
    }
    Glob::new(marker).is_ok_and(|glob| {
        let matcher = glob.compile_matcher();
        names.iter().any(|name| matcher.is_match(name))
    })
}

/// Detect canonical language IDs (`"rust"`, `"typescript"`, `"python"`, ...)
/// matching `names` against `catalog`.
pub fn detect_language_ids(names: &HashSet<String>, catalog: &[Language]) -> Vec<String> {
    let mut ids = Vec::new();
    for lang in catalog {
        if lang.markers.iter().any(|m| matches_marker(names, m))
            && !ids.iter().any(|id| id == &lang.id)
        {
            ids.push(lang.id.clone());
        }
    }
    ids
}

/// Detect canonical language IDs present in `folder` using the built-in catalog.
pub fn local_language_ids(folder: &Path) -> Vec<String> {
    let Ok(names) = project_filenames(folder) else {
        return Vec::new();
    };
    let catalog = builtin_languages();
    detect_language_ids(&names, &catalog)
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct ProjectLanguages {
    pub standard: Vec<String>,
    pub more: Vec<String>,
    pub local: Vec<String>,
}

impl ProjectLanguages {
    pub fn detect(names: &HashSet<String>, catalog: &[Language]) -> Self {
        let mut found = Self::default();
        for lang in catalog {
            if lang.markers.iter().any(|m| matches_marker(names, m)) {
                let group = if lang.more {
                    &mut found.more
                } else {
                    &mut found.standard
                };
                group.push(lang.name.clone());
            }
        }
        // The server can add or retire engines independently of a client release.
        // Recognize manifests for known local-only ecosystems and retired engines.
        for lang in builtin_languages()
            .iter()
            .filter(|l| !catalog.iter().any(|c| c.id == l.id))
        {
            if lang.markers.iter().any(|m| matches_marker(names, m)) {
                found.local.push(lang.name.clone());
            }
        }
        for (name, markers) in [
            ("Scala", &["build.sbt", "*.scala"][..]),
            (
                "Haskell",
                &["stack.yaml", "cabal.project", "*.cabal", "*.hs"],
            ),
            ("Clojure", &["deps.edn", "project.clj"]),
            ("Erlang", &["rebar.config"]),
            ("R", &["*.Rproj", "renv.lock", "*.R"]),
            ("Julia", &["JuliaProject.toml", "*.jl"]),
            ("Lua", &["*.rockspec", "*.lua"]),
            ("OCaml", &["dune-project", "*.opam"]),
            ("Elm", &["elm.json"]),
            ("Perl", &["cpanfile", "Makefile.PL"]),
            ("Terraform", &["*.tf"]),
        ] {
            if !catalog.iter().any(|lang| lang.name == name)
                && markers.iter().any(|m| matches_marker(names, m))
            {
                found.local.push(name.into());
            }
        }
        found
    }

    /// (enable checks, include more engines) when Enter accepts the wizard choice.
    pub fn recommendation(&self) -> (bool, bool) {
        (
            !self.standard.is_empty() || !self.more.is_empty(),
            !self.more.is_empty(),
        )
    }

    pub fn summary(&self) -> String {
        let mut groups = Vec::new();
        for (group, label) in [
            (&self.standard, "runs remotely"),
            (&self.more, "runs remotely with extra engines"),
            (&self.local, "runs on your machine"),
        ] {
            if !group.is_empty() {
                groups.push(format!("{} ({label})", group.join(", ")));
            }
        }
        if groups.is_empty() {
            "No supported project markers found.".into()
        } else {
            groups.join(" · ")
        }
    }
}

impl RemoteChecks {
    pub fn state(&self) -> &'static str {
        if self.enabled {
            "on"
        } else if self.available {
            "off"
        } else {
            "unavailable for this account"
        }
    }

    /// Returns a clear explanation when the user asked to turn remote checks
    /// off (`requested_enabled == Some(false)`) but the account stayed enabled
    /// because of server-side account policy.
    pub fn policy_locked_off_notice(
        &self,
        requested_enabled: Option<bool>,
    ) -> Option<&'static str> {
        if requested_enabled == Some(false) && self.enabled {
            Some(
                "Remote checks remain on because this account is enabled by account policy and cannot be turned off via the account toggle. To disable remote checks locally, set CONTEXTSTREAM_HOSTED_TOOLS=off.",
            )
        } else {
            None
        }
    }

    pub fn price_line(&self) -> String {
        match &self.pricing {
            Some(price) if !self.credits.as_ref().is_some_and(|c| c.no_charge) => format!(
                "{} {} per 10 s of run time. Exact repeats and checks that run on your machine are free.",
                price.credits_per_10s, price.unit
            ),
            _ => "Remote checks are free for this account.".into(),
        }
    }

    pub fn balance_line(&self) -> Option<String> {
        let credits = self.credits.as_ref()?;
        if credits.no_charge {
            return Some("Included with your account".into());
        }
        let available = credits.available?.to_string();
        let mut balance = String::new();
        for (index, ch) in available.chars().enumerate() {
            if index > 0 && (available.len() - index).is_multiple_of(3) && !balance.ends_with('-') {
                balance.push(',');
            }
            balance.push(ch);
        }
        Some(format!("You have {balance} credits"))
    }

    pub fn status_text(&self, project: Option<&ProjectLanguages>) -> String {
        let more = match self.more_engines {
            Some(true) => "on",
            Some(false) => "off",
            None => "unknown",
        };
        let state_label = if self.enabled && self.policy_locked {
            "on (account policy)"
        } else {
            self.state()
        };
        let mut lines = vec![
            format!("Remote checks: {state_label} · Extra engines: {more}"),
            self.price_line(),
        ];
        lines.extend(self.balance_line());
        for (extra, label) in [(false, "Standard engines"), (true, "Extra engines")] {
            let names: Vec<_> = self
                .languages
                .iter()
                .filter(|l| l.more == extra)
                .map(|l| l.name.as_str())
                .collect();
            if !names.is_empty() {
                lines.push(format!("{label}: {}", names.join(", ")));
            }
        }
        lines.push(format!(
            "This project: {}",
            project
                .map(ProjectLanguages::summary)
                .unwrap_or_else(|| "language information unavailable.".into())
        ));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_project_markers_in_root_and_children_only() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "Cargo.toml",
            "frontend/package.json",
            "app/Core.csproj",
            "app/Tests.fsproj",
            "nested/deeper/go.mod",
            "scripts/project.clj",
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        for skip in SKIP_DIRS {
            let dir = root.path().join(skip);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("Gemfile"), "").unwrap();
        }
        let names = project_filenames(root.path()).unwrap();
        let actual = ProjectLanguages::detect(&names, &builtin_languages());
        assert_eq!(actual.standard, ["Rust", "TypeScript and JavaScript"]);
        assert_eq!(actual.more, [".NET"]);
        assert_eq!(actual.local, ["Clojure"]);
        assert_eq!(actual.recommendation(), (true, true));
        assert_eq!(
            detect_language_ids(&names, &builtin_languages()),
            ["rust", "typescript", "dotnet"]
        );
        assert_eq!(
            local_language_ids(root.path()),
            ["rust", "typescript", "dotnet"]
        );
        assert_eq!(indexed_filename("lib/Core.csproj"), Some("Core.csproj"));
        assert_eq!(indexed_filename("node_modules/Gemfile"), None);
        assert_eq!(indexed_filename("a/b/go.mod"), None);
    }

    #[test]
    fn server_catalog_controls_detection_and_pricing() {
        let mut state: RemoteChecks = serde_json::from_value(json!({
            "tool":"check", "enabled":false, "available":true, "more_engines":false,
            "languages":[{"id":"sample","name":"Sample","tools":"sample test","more":false,"markers":["*.sample"]}],
            "pricing":{"credits_per_10s":20,"unit":"Context Credits","billing":"run_time","free":["cache hits"]},
            "credits":{"available":41200,"no_charge":false}
        })).unwrap();
        let files = ["Cargo.toml".into(), "app.sample".into()].into();
        let project = ProjectLanguages::detect(&files, &state.languages);
        assert_eq!(project.standard, ["Sample"]);
        assert_eq!(project.local, ["Rust"]);
        assert_eq!(project.recommendation(), (true, false));
        let status = state.status_text(Some(&project));
        for phrase in [
            "Remote checks: off · Extra engines: off",
            "20 Context Credits per 10 s of run time",
            "You have 41,200 credits",
            "Sample (runs remotely)",
            "Rust (runs on your machine)",
        ] {
            assert!(status.contains(phrase), "{status}");
        }
        state.credits.as_mut().unwrap().no_charge = true;
        assert_eq!(
            state.balance_line().as_deref(),
            Some("Included with your account")
        );
        assert_eq!(
            state.price_line(),
            "Remote checks are free for this account."
        );
        state.credits = None;
        state.pricing = None;
        assert!(state.balance_line().is_none());
        assert_eq!(
            state.price_line(),
            "Remote checks are free for this account."
        );
    }

    #[test]
    fn policy_locked_off_notice_explains_account_policy_and_local_override() {
        let locked: RemoteChecks = serde_json::from_value(json!({
            "tool": "check",
            "enabled": true,
            "available": true,
            "policy_locked": true,
            "more_engines": false
        }))
        .unwrap();
        assert!(locked.policy_locked);
        assert!(locked
            .status_text(None)
            .contains("Remote checks: on (account policy)"));
        let notice = locked
            .policy_locked_off_notice(Some(false))
            .expect("policy-locked off must return a clear notice");
        assert!(notice.contains("account policy"));
        assert!(notice.contains("CONTEXTSTREAM_HOSTED_TOOLS=off"));
        assert!(locked.policy_locked_off_notice(Some(true)).is_none());
        assert!(locked.policy_locked_off_notice(None).is_none());

        // Also catches older APIs that omit `policy_locked` yet keep `enabled: true`.
        let older_api: RemoteChecks = serde_json::from_value(json!({
            "tool": "check",
            "enabled": true,
            "available": true,
            "more_engines": false
        }))
        .unwrap();
        assert!(!older_api.policy_locked);
        assert!(older_api.policy_locked_off_notice(Some(false)).is_some());
    }
}
