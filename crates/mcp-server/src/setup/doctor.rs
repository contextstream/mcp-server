//! `contextstream-mcp doctor` — validate editor integration health.
//!
//! For every explicitly selected or setup-configured editor, verifies the surfaces setup manages
//! exist and parse: MCP config (global + project) with a `contextstream`
//! entry of the editor-correct shape, rules files carrying the managed
//! marker, hook files/scripts, and local/remote readiness evidence. Every
//! repair command is editor-scoped. `--json` emits a stable machine-readable
//! v2 local report for install scripts. `--support` emits a separate
//! privacy-bounded report with no credentials or local paths. The process exit
//! code is non-zero when any check fails so scripts can gate on it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::style;
use futures::future::join_all;
use mcp_client::{
    harness_readiness::{
        effective_evidence_records_for, highest_effective_stage, read_harness_readiness,
        CurrentHarnessVersions, HarnessReadinessLedger,
    },
    harness_remote::HarnessReadinessStatusResponse,
    ContextStreamClient,
};
use mcp_types::{
    config::DEFAULT_API_URL, Config, Error as McpError, HarnessId, HarnessReadinessEvidence,
    HarnessReadinessStage, McpTransportSupport, ReadinessEvidenceSource, ReadinessEvidenceStatus,
    TeachingLoadEvidence, HARNESS_TEACHING_VERSION,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::editors::Editor;
use super::mcp_config::{try_parse_json_like, MANAGED_CONFIG_VERSION};
use super::rules::content_has_owned_contextstream_rules;

pub const DOCTOR_REPORT_SCHEMA_VERSION: u16 = 2;
const DOCTOR_COMPLETION_SUMMARY_SCHEMA_VERSION: u16 = 1;
const DOCTOR_SUPPORT_REPORT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorTransportState {
    Hosted,
    Local,
    LegacyUnknown,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorScope {
    Global,
    Project,
    All,
}

impl DoctorScope {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "global" => Ok(Self::Global),
            "project" => Ok(Self::Project),
            "all" => Ok(Self::All),
            _ => anyhow::bail!(
                "Unknown doctor scope '{}'. Supported values: global, project, all.",
                value
            ),
        }
    }

    const fn include_global(self) -> bool {
        matches!(self, Self::Global | Self::All)
    }

    const fn include_project(self) -> bool {
        matches!(self, Self::Project | Self::All)
    }

    const fn as_cli_value(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::All => "all",
        }
    }
}

impl DoctorTransportState {
    const fn as_support_value(self) -> &'static str {
        match self {
            Self::Hosted => "hosted_remote",
            Self::Local => "local_recovery",
            Self::LegacyUnknown => "legacy_unknown",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub project_path: Option<PathBuf>,
    pub explicit_editors: Option<Vec<Editor>>,
    pub only_configured: bool,
    pub scope: DoctorScope,
    pub scope_was_explicit: bool,
    pub repair: bool,
    pub dry_run: bool,
}

impl DoctorOptions {
    pub fn new(project_path: Option<PathBuf>) -> Self {
        Self {
            project_path,
            explicit_editors: None,
            only_configured: false,
            scope: DoctorScope::All,
            scope_was_explicit: false,
            repair: false,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct SurfaceCheck {
    pub surface: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl SurfaceCheck {
    fn pass(surface: &'static str, detail: impl Into<String>, path: Option<&Path>) -> Self {
        Self {
            surface,
            status: CheckStatus::Pass,
            detail: detail.into(),
            path: path.map(|p| p.display().to_string()),
            fix: None,
        }
    }

    fn skipped(surface: &'static str, detail: impl Into<String>) -> Self {
        Self {
            surface,
            status: CheckStatus::Skipped,
            detail: detail.into(),
            path: None,
            fix: None,
        }
    }

    fn warning(
        surface: &'static str,
        detail: impl Into<String>,
        path: Option<&Path>,
        fix: Option<&str>,
    ) -> Self {
        Self {
            surface,
            status: CheckStatus::Warn,
            detail: detail.into(),
            path: path.map(|p| p.display().to_string()),
            fix: fix.map(str::to_string),
        }
    }

    fn failure(
        surface: &'static str,
        detail: impl Into<String>,
        path: Option<&Path>,
        fix: Option<&str>,
    ) -> Self {
        Self {
            surface,
            status: CheckStatus::Fail,
            detail: detail.into(),
            path: path.map(|p| p.display().to_string()),
            fix: fix.map(str::to_string),
        }
    }

    fn problem(
        surface: &'static str,
        status: CheckStatus,
        detail: impl Into<String>,
        path: Option<&Path>,
        fix: &str,
    ) -> Self {
        Self {
            surface,
            status,
            detail: detail.into(),
            path: path.map(|p| p.display().to_string()),
            fix: Some(fix.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessSnapshotState {
    Ready,
    Partial,
    Stale,
    Missing,
    NotObservable,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessStageReport {
    pub stage: HarnessReadinessStage,
    pub status: ReadinessEvidenceStatus,
    pub evidence: Vec<HarnessReadinessEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessSnapshot {
    pub state: ReadinessSnapshotState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highest_ready_stage: Option<HarnessReadinessStage>,
    pub stages: Vec<ReadinessStageReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HarnessReadinessReport {
    pub harness_id: HarnessId,
    pub teaching_load_evidence: TeachingLoadEvidence,
    pub local: ReadinessSnapshot,
    pub remote: ReadinessSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditorReport {
    pub editor: &'static str,
    pub editor_name: &'static str,
    pub checks: Vec<SurfaceCheck>,
    pub readiness: HarnessReadinessReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorTargeting {
    pub scope: DoctorScope,
    pub source: &'static str,
    pub editors: Vec<&'static str>,
    pub detection_fallback_allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairChange {
    pub path: String,
    pub action: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorRepairReport {
    pub mode: &'static str,
    pub operations: Vec<&'static str>,
    pub changes: Vec<RepairChange>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub schema_version: u16,
    pub targeting: DoctorTargeting,
    pub editors: Vec<EditorReport>,
    pub installation: SurfaceCheck,
    pub credentials: SurfaceCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<DoctorRepairReport>,
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActivationGuidanceState {
    RepairRequired,
    RulesOnly,
    RestartRequired,
    ConnectionUnverified,
    ConnectionObserved,
    GroundingObserved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorActivationGuidance {
    editor_name: &'static str,
    state: ActivationGuidanceState,
    status: String,
    next_step: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorSupportRecommendedAction {
    RepairManagedSurfaces,
    ReloadRules,
    ReloadEditor,
    ReloadAndRetryDoctor,
    VerifyCheckoutIndex,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportEditor {
    editor: &'static str,
    activation_state: ActivationGuidanceState,
    recommended_action: DoctorSupportRecommendedAction,
    remote_readiness: ReadinessSnapshotState,
    #[serde(skip_serializing_if = "Option::is_none")]
    highest_server_stage: Option<HarnessReadinessStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repair_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorSupportProjectState {
    NotChecked,
    Missing,
    Unreadable,
    BindingMismatch,
    Bound,
    ExactCheckout,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportProject {
    state: DoctorSupportProjectState,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_locator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_fingerprint: Option<String>,
}

impl DoctorSupportProject {
    const fn with_state(state: DoctorSupportProjectState) -> Self {
        Self {
            state,
            workspace_id: None,
            project_id: None,
            checkout_id: None,
            checkout_locator: None,
            repository_fingerprint: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportBridge {
    required: bool,
    registration_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    registration_state: Option<super::SyncBridgeRegistrationState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activation_state: Option<super::SyncBridgeActivationState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    health_state: Option<crate::watch::SyncBridgeHealthState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    heartbeat_fresh: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportTargeting {
    scope: DoctorScope,
    source: &'static str,
    editors: Vec<&'static str>,
    detection_fallback_allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportSummary {
    installation: CheckStatus,
    credentials: CheckStatus,
    pass: usize,
    warn: usize,
    fail: usize,
    skipped: usize,
    setup_healthy: bool,
    doctor_has_failures: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorSupportReport {
    schema_version: u16,
    source_report_schema_version: u16,
    mcp_version: &'static str,
    transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    installation_id: Option<Uuid>,
    targeting: DoctorSupportTargeting,
    editors: Vec<DoctorSupportEditor>,
    project: DoctorSupportProject,
    sync_bridge: DoctorSupportBridge,
    summary: DoctorSupportSummary,
}

/// Privacy-bounded projection used by setup completion telemetry.
///
/// The interactive/JSON doctor report intentionally carries local paths and
/// actionable diagnostics. Those belong on the user's machine and must not be
/// copied into the setup device record. Keep this projection to closed enums
/// and aggregate counts: no paths, details, fixes, evidence records, or
/// timestamps.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorCompletionSummary {
    pub schema_version: u16,
    pub source_report_schema_version: u16,
    pub editors: Vec<DoctorCompletionEditorSummary>,
    pub installation: CheckStatus,
    pub credentials: CheckStatus,
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
    pub has_failures: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorCompletionEditorSummary {
    pub editor: &'static str,
    pub checks: DoctorCompletionCheckCounts,
    pub local_readiness: ReadinessSnapshotState,
    pub remote_readiness: ReadinessSnapshotState,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DoctorCompletionCheckCounts {
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
}

impl DoctorReport {
    pub fn has_failures(&self) -> bool {
        self.fail > 0
            || self
                .repair
                .as_ref()
                .is_some_and(|repair| !repair.failures.is_empty())
    }

    /// Whether setup-owned configuration is broken.
    ///
    /// Runtime readiness is deliberately excluded. Missing or stale handshake
    /// evidence is expected immediately after setup changes a client config,
    /// and must lead to reload guidance rather than a false repair claim.
    pub(crate) fn has_setup_failures(&self) -> bool {
        self.installation.status == CheckStatus::Fail
            || self.credentials.status == CheckStatus::Fail
            || self.editors.iter().any(|editor| {
                editor.checks.iter().any(|check| {
                    check.status == CheckStatus::Fail
                        && !matches!(check.surface, "readiness_local" | "readiness_remote")
                })
            })
            || self
                .repair
                .as_ref()
                .is_some_and(|repair| !repair.failures.is_empty())
    }

    pub(crate) fn completion_summary(&self) -> DoctorCompletionSummary {
        let editors = self
            .editors
            .iter()
            .map(|editor| {
                let mut checks = DoctorCompletionCheckCounts::default();
                for check in &editor.checks {
                    match check.status {
                        CheckStatus::Pass => checks.pass += 1,
                        CheckStatus::Warn => checks.warn += 1,
                        CheckStatus::Fail => checks.fail += 1,
                        CheckStatus::Skipped => checks.skipped += 1,
                    }
                }
                DoctorCompletionEditorSummary {
                    editor: editor.editor,
                    checks,
                    local_readiness: editor.readiness.local.state,
                    remote_readiness: editor.readiness.remote.state,
                }
            })
            .collect();

        DoctorCompletionSummary {
            schema_version: DOCTOR_COMPLETION_SUMMARY_SCHEMA_VERSION,
            source_report_schema_version: self.schema_version,
            editors,
            installation: self.installation.status,
            credentials: self.credentials.status,
            pass: self.pass,
            warn: self.warn,
            fail: self.fail,
            skipped: self.skipped,
            has_failures: self.has_failures(),
        }
    }
}

/// Root key holding MCP server entries in this editor's JSON config.
fn mcp_root_key(editor: &Editor) -> &'static str {
    match editor {
        Editor::Copilot => "servers",
        Editor::OpenCode | Editor::KiloCode => "mcp",
        _ => "mcpServers",
    }
}

/// Locate the contextstream server entry in a parsed editor config.
fn contextstream_entry<'a>(editor: &Editor, config: &'a Value) -> Option<&'a Value> {
    if editor.uses_vscode_settings() {
        let key = match editor {
            Editor::Cline => "cline.mcpServers",
            Editor::RooCode => "roo-cline.mcpServers",
            _ => return None,
        };
        return config
            .get(key)
            .and_then(|servers| servers.get("contextstream"));
    }
    config
        .get(mcp_root_key(editor))
        .and_then(|servers| servers.get("contextstream"))
}

fn json_identity_value<'a>(entry: &'a Value, env_name: &str, header_name: &str) -> Option<&'a str> {
    ["headers", "env", "environment"]
        .into_iter()
        .find_map(|container| {
            let key = if container == "headers" {
                header_name
            } else {
                env_name
            };
            entry.get(container)?.get(key)?.as_str()
        })
}

fn validate_managed_identity_values(
    editor: &Editor,
    client: Option<&str>,
    managed_config_version: Option<&str>,
    teaching_version: Option<&str>,
    installation_id: Option<&str>,
    expected_installation_id: Option<Uuid>,
) -> std::result::Result<(), String> {
    if client != Some(editor.harness_id().as_str()) {
        return Err(format!(
            "managed client identity is missing or is not {}",
            editor.harness_id().as_str()
        ));
    }
    if managed_config_version != Some(MANAGED_CONFIG_VERSION) {
        return Err(format!(
            "managed config version is missing or stale (expected {})",
            MANAGED_CONFIG_VERSION
        ));
    }
    if teaching_version != Some(HARNESS_TEACHING_VERSION) {
        return Err(format!(
            "teaching version is missing or stale (expected {})",
            HARNESS_TEACHING_VERSION
        ));
    }
    let installation_id = installation_id
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|value| !value.is_nil())
        .ok_or_else(|| "managed installation identity is missing or invalid".to_string())?;
    if expected_installation_id.is_some_and(|expected| expected != installation_id) {
        return Err("managed installation identity does not match this installation".to_string());
    }
    Ok(())
}

fn validate_json_managed_identity(
    editor: &Editor,
    entry: &Value,
    expected_installation_id: Option<Uuid>,
) -> std::result::Result<(), String> {
    validate_managed_identity_values(
        editor,
        json_identity_value(entry, "CONTEXTSTREAM_CLIENT", "X-ContextStream-Client"),
        json_identity_value(
            entry,
            "CONTEXTSTREAM_MANAGED_CONFIG_VERSION",
            "X-ContextStream-Managed-Config-Version",
        ),
        json_identity_value(
            entry,
            "CONTEXTSTREAM_TEACHING_VERSION",
            "X-ContextStream-Teaching-Version",
        ),
        json_identity_value(
            entry,
            "CONTEXTSTREAM_INSTALLATION_ID",
            "X-ContextStream-Installation-Id",
        ),
        expected_installation_id,
    )
}

fn check_codex_mcp_config(
    surface: &'static str,
    path: &Path,
    content: &str,
    expected_installation_id: Option<Uuid>,
    fix: &str,
) -> SurfaceCheck {
    let document = match super::mcp_config::parse_codex_toml(content, path) {
        Ok(document) => document,
        Err(error) => {
            return SurfaceCheck::problem(
                surface,
                CheckStatus::Fail,
                format!("does not parse: {error}"),
                Some(path),
                fix,
            )
        }
    };
    let Some(entry) = super::mcp_config::contextstream_toml_item(&document) else {
        return SurfaceCheck::problem(
            surface,
            CheckStatus::Fail,
            "no [mcp_servers.contextstream] table",
            Some(path),
            fix,
        );
    };
    let shape = if super::mcp_config::toml_item_string(entry, "url").is_some_and(|v| !v.is_empty())
    {
        "remote url configured"
    } else if super::mcp_config::toml_item_string(entry, "command").is_some_and(|v| !v.is_empty()) {
        "local command configured"
    } else {
        return SurfaceCheck::problem(
            surface,
            CheckStatus::Fail,
            "contextstream table has neither a non-empty url nor command",
            Some(path),
            fix,
        );
    };
    let nested = |env_name: &str, header_name: &str| {
        super::mcp_config::toml_nested_string(entry, "env", env_name).or_else(|| {
            ["http_headers", "headers"]
                .into_iter()
                .find_map(|table| super::mcp_config::toml_nested_string(entry, table, header_name))
        })
    };
    match validate_managed_identity_values(
        &Editor::Codex,
        nested("CONTEXTSTREAM_CLIENT", "X-ContextStream-Client"),
        nested(
            "CONTEXTSTREAM_MANAGED_CONFIG_VERSION",
            "X-ContextStream-Managed-Config-Version",
        ),
        nested(
            "CONTEXTSTREAM_TEACHING_VERSION",
            "X-ContextStream-Teaching-Version",
        ),
        nested(
            "CONTEXTSTREAM_INSTALLATION_ID",
            "X-ContextStream-Installation-Id",
        ),
        expected_installation_id,
    ) {
        Ok(()) => SurfaceCheck::pass(
            surface,
            format!("{shape}; managed identity current"),
            Some(path),
        ),
        Err(detail) => SurfaceCheck::problem(surface, CheckStatus::Fail, detail, Some(path), fix),
    }
}

/// Validate the transport shape of a contextstream server entry for `editor`.
/// Returns Err(detail) when the entry cannot work as written.
fn validate_entry_shape(editor: &Editor, entry: &Value) -> std::result::Result<String, String> {
    if matches!(editor, Editor::KiloCode) {
        // Kilo only accepts type local|remote (kilo.ai docs); the generic
        // "http" transport name was the silent-breakage bug fixed alongside
        // this doctor.
        return match entry.get("type").and_then(Value::as_str) {
            Some("local") => match entry.get("command").and_then(Value::as_array) {
                Some(command) if !command.is_empty() => Ok("local command configured".to_string()),
                _ => Err("type=local but command array is missing/empty".to_string()),
            },
            Some("remote") => match entry.get("url").and_then(Value::as_str) {
                Some(url) if !url.is_empty() => Ok(format!("remote → {}", url)),
                _ => Err("type=remote but url is missing".to_string()),
            },
            Some(other) => Err(format!(
                "type=\"{}\" is invalid for Kilo (must be local|remote)",
                other
            )),
            None => Err("entry has no type field (Kilo needs local|remote)".to_string()),
        };
    }

    // Generic editors: a usable entry has either a remote url/serverUrl or a
    // local command whose binary exists on disk.
    if let Some(url) = entry
        .get("serverUrl")
        .or_else(|| entry.get("url"))
        .and_then(Value::as_str)
    {
        if url.is_empty() {
            return Err("remote entry has an empty url".to_string());
        }
        return Ok(format!("remote → {}", url));
    }

    if let Some(command) = entry.get("command") {
        let binary = match command {
            Value::String(s) => Some(s.clone()),
            Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
            _ => None,
        };
        return match binary {
            Some(binary) if !binary.trim().is_empty() => {
                let bare = binary.trim_matches('"');
                if Path::new(bare).is_absolute() && !Path::new(bare).exists() {
                    Err(format!("local binary not found: {}", bare))
                } else {
                    Ok("local command configured".to_string())
                }
            }
            _ => Err("local entry has an empty command".to_string()),
        };
    }

    Err("entry has neither a url/serverUrl nor a command".to_string())
}

/// Check one JSON-like MCP config file for a working contextstream entry.
fn check_mcp_config_file(
    editor: &Editor,
    surface: &'static str,
    path: &Path,
    missing_status: CheckStatus,
    expected_installation_id: Option<Uuid>,
    fix: &str,
) -> SurfaceCheck {
    if !path.exists() {
        return SurfaceCheck::problem(
            surface,
            missing_status,
            "config file missing",
            Some(path),
            fix,
        );
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            return SurfaceCheck::problem(
                surface,
                CheckStatus::Fail,
                format!("unreadable: {}", e),
                Some(path),
                fix,
            )
        }
    };

    if matches!(editor, Editor::Codex) {
        return check_codex_mcp_config(surface, path, &content, expected_installation_id, fix);
    }

    let parsed = match try_parse_json_like(&content) {
        Ok(parsed) => parsed,
        Err(e) => {
            return SurfaceCheck::problem(
                surface,
                CheckStatus::Fail,
                format!("does not parse: {}", e),
                Some(path),
                fix,
            )
        }
    };

    match contextstream_entry(editor, &parsed) {
        Some(entry) => match validate_entry_shape(editor, entry) {
            Ok(detail) => {
                match validate_json_managed_identity(editor, entry, expected_installation_id) {
                    Ok(()) => SurfaceCheck::pass(
                        surface,
                        format!("{detail}; managed identity current"),
                        Some(path),
                    ),
                    Err(identity) => {
                        SurfaceCheck::problem(surface, CheckStatus::Fail, identity, Some(path), fix)
                    }
                }
            }
            Err(detail) => {
                SurfaceCheck::problem(surface, CheckStatus::Fail, detail, Some(path), fix)
            }
        },
        None => SurfaceCheck::problem(
            surface,
            CheckStatus::Fail,
            "no contextstream server entry",
            Some(path),
            fix,
        ),
    }
}

/// Check a rules file for the managed ContextStream block.
fn check_rules_file(
    surface: &'static str,
    path: &Path,
    missing_status: CheckStatus,
    fix: &str,
) -> SurfaceCheck {
    if !path.exists() {
        return SurfaceCheck::problem(
            surface,
            missing_status,
            "rules file missing",
            Some(path),
            fix,
        );
    }
    match std::fs::read_to_string(path) {
        Ok(content) if content_has_owned_contextstream_rules(&content) => {
            let installed_hash = mcp_types::rules_hash::extract_hash_marker(&content);
            let canonical_hash = mcp_types::rules_hash::canonical_rules_hash();
            match (installed_hash.as_deref(), canonical_hash) {
                (Some(installed), Some(canonical)) if installed != canonical => {
                    SurfaceCheck::problem(
                        surface,
                        missing_status,
                        "managed ContextStream rules are stale",
                        Some(path),
                        fix,
                    )
                }
                (None, Some(_)) => SurfaceCheck::problem(
                    surface,
                    missing_status,
                    "managed ContextStream rules predate content-hash verification",
                    Some(path),
                    fix,
                ),
                _ => SurfaceCheck::pass(
                    surface,
                    "managed ContextStream rules present and current",
                    Some(path),
                ),
            }
        }
        Ok(_) => SurfaceCheck::problem(
            surface,
            missing_status,
            "file exists but has no ContextStream block",
            Some(path),
            fix,
        ),
        Err(e) => SurfaceCheck::problem(
            surface,
            CheckStatus::Fail,
            format!("unreadable: {}", e),
            Some(path),
            fix,
        ),
    }
}

/// Cascade hook entry fields documented at docs.devin.ai/desktop/cascade/hooks.
fn windsurf_entry_keys_valid(entry: &Value) -> bool {
    entry
        .as_object()
        .map(|obj| {
            obj.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "command" | "powershell" | "show_output" | "working_directory"
                )
            })
        })
        .unwrap_or(false)
}

/// Check the hooks surface for one editor.
fn check_hooks(editor: &Editor) -> SurfaceCheck {
    const SURFACE: &str = "hooks";
    let fix = format!(
        "contextstream-mcp update-hooks --scope global --editors {}",
        editor.id()
    );

    if !editor.has_hooks() {
        return SurfaceCheck::skipped(
            SURFACE,
            match editor {
                Editor::KiloCode => "Kilo has no hook system (content watcher covers freshness)",
                _ => "editor has no hook lifecycle",
            },
        );
    }

    let home = match dirs::home_dir() {
        Some(home) => home,
        None => {
            return SurfaceCheck::problem(
                SURFACE,
                CheckStatus::Fail,
                "cannot resolve home directory",
                None,
                &fix,
            )
        }
    };

    match editor {
        Editor::ClaudeCode => {
            let path = home.join(".claude").join("settings.json");
            check_hook_file_has_owned_contextstream(editor, &path, &fix)
        }
        Editor::Cursor => {
            let path = home.join(".cursor").join("hooks.json");
            check_hook_file_has_owned_contextstream(editor, &path, &fix)
        }
        Editor::Windsurf => {
            let path = home.join(".codeium").join("windsurf").join("hooks.json");
            let base = check_hook_file_has_owned_contextstream(editor, &path, &fix);
            if base.status != CheckStatus::Pass {
                return base;
            }
            // Schema hygiene: entries must only carry documented Cascade keys
            // (a `type: "command"` leak or missing powershell-on-Windows means
            // hooks silently never fire).
            let parsed = match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|content| try_parse_json_like(&content))
            {
                Ok(parsed) => parsed,
                Err(error) => {
                    return SurfaceCheck::problem(
                        SURFACE,
                        CheckStatus::Fail,
                        format!("could not revalidate hook schema: {error}"),
                        Some(&path),
                        &fix,
                    )
                }
            };
            let all_valid = parsed
                .get("hooks")
                .and_then(Value::as_object)
                .map(|events| {
                    events.values().all(|entries| {
                        entries
                            .as_array()
                            .map(|list| {
                                list.iter()
                                    .filter(|entry| {
                                        entry
                                            .get("command")
                                            .and_then(Value::as_str)
                                            .map(super::hooks::is_owned_contextstream_hook_command)
                                            .unwrap_or(false)
                                    })
                                    .all(windsurf_entry_keys_valid)
                            })
                            .unwrap_or(true)
                    })
                })
                .unwrap_or(false);
            if all_valid {
                base
            } else {
                SurfaceCheck::problem(
                    SURFACE,
                    CheckStatus::Fail,
                    "contextstream entries carry undocumented Cascade fields",
                    Some(&path),
                    &fix,
                )
            }
        }
        Editor::Cline | Editor::RooCode => {
            let dir = if matches!(editor, Editor::Cline) {
                home.join("Documents")
                    .join("Cline")
                    .join("Rules")
                    .join("Hooks")
            } else {
                home.join(".roo").join("hooks")
            };
            match super::hooks::validate_managed_wrapper_set(&dir) {
                Ok(count) => SurfaceCheck::pass(
                    SURFACE,
                    format!("{count} current managed wrapper scripts installed"),
                    Some(&dir),
                ),
                Err(detail) => {
                    SurfaceCheck::problem(SURFACE, CheckStatus::Fail, detail, Some(&dir), &fix)
                }
            }
        }
        _ => SurfaceCheck::skipped(SURFACE, "no hook check for this editor"),
    }
}

fn check_hook_file_has_owned_contextstream(
    editor: &Editor,
    path: &Path,
    fix: &str,
) -> SurfaceCheck {
    const SURFACE: &str = "hooks";
    if !path.exists() {
        return SurfaceCheck::problem(
            SURFACE,
            CheckStatus::Fail,
            "hooks file missing",
            Some(path),
            fix,
        );
    }
    match std::fs::read_to_string(path) {
        Ok(content) => match try_parse_json_like(&content) {
            Ok(value) => match super::hooks::validate_managed_hook_config(editor, &value) {
                Ok(count) => SurfaceCheck::pass(
                    SURFACE,
                    format!("{count} current managed hook entries installed"),
                    Some(path),
                ),
                Err(detail) => {
                    SurfaceCheck::problem(SURFACE, CheckStatus::Fail, detail, Some(path), fix)
                }
            },
            Err(error) => SurfaceCheck::problem(
                SURFACE,
                CheckStatus::Fail,
                format!("does not parse: {error}"),
                Some(path),
                fix,
            ),
        },
        Err(e) => SurfaceCheck::problem(
            SURFACE,
            CheckStatus::Fail,
            format!("unreadable: {}", e),
            Some(path),
            fix,
        ),
    }
}

/// Run scoped filesystem checks for one editor (no network).
fn check_editor_surfaces(
    editor: &Editor,
    project_path: Option<&Path>,
    scope: DoctorScope,
    expected_installation_id: Option<Uuid>,
) -> Vec<SurfaceCheck> {
    let mut checks = Vec::new();

    // Global MCP config.
    if scope.include_global() {
        if matches!(editor, Editor::Aider) {
            checks.push(SurfaceCheck::skipped(
                "mcp_global",
                "Aider does not use MCP",
            ));
        } else {
            let fix = format!(
                "contextstream-mcp update-configs --scope global --editors {}",
                editor.id()
            );
            match editor.mcp_config_path() {
                Some(path) => checks.push(check_mcp_config_file(
                    editor,
                    "mcp_global",
                    &path,
                    CheckStatus::Fail,
                    expected_installation_id,
                    &fix,
                )),
                None => checks.push(SurfaceCheck::skipped("mcp_global", "no global config path")),
            }
        }
    }

    // Project MCP config (optional surface — global keeps the editor working).
    if scope.include_project() {
        if let Some(project) = project_path {
            if editor.supports_project_mcp_config() {
                if let Some(path) = editor.project_mcp_config_path(project) {
                    let fix = format!(
                        "contextstream-mcp update-configs --scope project --editors {}",
                        editor.id()
                    );
                    checks.push(check_mcp_config_file(
                        editor,
                        "mcp_project",
                        &path,
                        CheckStatus::Warn,
                        expected_installation_id,
                        &fix,
                    ));
                }
            }

            // Project rules.
            if let Some(path) = editor.rules_path(Some(project)) {
                let fix = format!(
                    "contextstream-mcp update-rules --scope project --editors {}",
                    editor.id()
                );
                checks.push(check_rules_file(
                    "rules_project",
                    &path,
                    CheckStatus::Fail,
                    &fix,
                ));
            }
        } else {
            checks.push(SurfaceCheck::problem(
                "project_scope",
                CheckStatus::Fail,
                "cannot resolve the current project directory",
                None,
                "run doctor from the project directory or use --scope global",
            ));
        }
    }

    // Global rules (some editors have none — skip silently via None).
    if scope.include_global() {
        if let Some(path) = editor.rules_path(None) {
            let fix = format!(
                "contextstream-mcp update-rules --scope global --editors {}",
                editor.id()
            );
            checks.push(check_rules_file(
                "rules_global",
                &path,
                CheckStatus::Warn,
                &fix,
            ));
        }

        checks.push(check_hooks(editor));
    }

    checks
}

/// Compatibility wrapper used by the compact post-setup summary.
pub fn check_editor(editor: &Editor, project_path: Option<&Path>) -> EditorReport {
    let installation_result = mcp_client::activation::existing_installation_id();
    let expected_installation_id = installation_result.as_ref().ok().copied().flatten();
    let scope = if project_path.is_some() {
        DoctorScope::All
    } else {
        DoctorScope::Global
    };
    let mut checks = check_editor_surfaces(editor, project_path, scope, expected_installation_id);
    if installation_result.is_err() {
        checks.insert(
            0,
            SurfaceCheck::failure(
                "installation",
                "installation identity is unreadable; managed config identity cannot be trusted",
                None,
                None,
            ),
        );
    } else if expected_installation_id.is_none() {
        checks.insert(
            0,
            SurfaceCheck::failure(
                "installation",
                "installation identity is missing; managed config identity cannot be correlated",
                None,
                Some("contextstream-mcp setup"),
            ),
        );
    }
    EditorReport {
        editor: editor.id(),
        editor_name: editor.display_name(),
        checks,
        readiness: HarnessReadinessReport {
            harness_id: editor.harness_id(),
            teaching_load_evidence: editor.profile().teaching_load_evidence,
            local: unavailable_readiness_snapshot(
                editor,
                "readiness is not queried during the compact setup summary",
            ),
            remote: unavailable_readiness_snapshot(
                editor,
                "readiness is not queried during the compact setup summary",
            ),
        },
    }
}

fn evidence_source_rank(source: ReadinessEvidenceSource) -> u8 {
    match source {
        ReadinessEvidenceSource::ManagedMcpConfig => 0,
        ReadinessEvidenceSource::ManagedRules => 1,
        ReadinessEvidenceSource::InstructionsLoadedHook => 2,
        ReadinessEvidenceSource::McpProtocolRequest => 3,
        ReadinessEvidenceSource::InitTool => 4,
        ReadinessEvidenceSource::ContextTool => 5,
        ReadinessEvidenceSource::ComplianceCheck => 6,
        ReadinessEvidenceSource::RuntimeBehavior => 7,
    }
}

fn default_missing_stage_status(
    editor: &Editor,
    stage: HarnessReadinessStage,
) -> ReadinessEvidenceStatus {
    let profile = editor.profile();
    match stage {
        HarnessReadinessStage::Configured
        | HarnessReadinessStage::Connected
        | HarnessReadinessStage::Grounded
            if profile.mcp_support == McpTransportSupport::None =>
        {
            ReadinessEvidenceStatus::NotObservable
        }
        HarnessReadinessStage::Loaded
            if profile.teaching_load_evidence == TeachingLoadEvidence::NotObservable =>
        {
            ReadinessEvidenceStatus::NotObservable
        }
        HarnessReadinessStage::Practicing
            if profile.teaching_load_evidence == TeachingLoadEvidence::NotObservable
                && !profile.dynamic_guidance =>
        {
            ReadinessEvidenceStatus::NotObservable
        }
        _ => ReadinessEvidenceStatus::Pending,
    }
}

fn strongest_stage_status(
    evidence: &[HarnessReadinessEvidence],
    fallback: ReadinessEvidenceStatus,
) -> ReadinessEvidenceStatus {
    for status in [
        ReadinessEvidenceStatus::Failed,
        ReadinessEvidenceStatus::Stale,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceStatus::Inferred,
        ReadinessEvidenceStatus::Pending,
        ReadinessEvidenceStatus::NotObservable,
    ] {
        if evidence.iter().any(|item| item.status == status) {
            return status;
        }
    }
    fallback
}

fn classify_readiness_snapshot(
    editor: &Editor,
    stages: &[ReadinessStageReport],
    highest_ready_stage: Option<HarnessReadinessStage>,
) -> ReadinessSnapshotState {
    if stages
        .iter()
        .any(|stage| stage.status == ReadinessEvidenceStatus::Failed)
    {
        return ReadinessSnapshotState::Failed;
    }
    if stages
        .iter()
        .any(|stage| stage.status == ReadinessEvidenceStatus::Stale)
    {
        return ReadinessSnapshotState::Stale;
    }
    let Some(highest) = highest_ready_stage else {
        return if stages
            .iter()
            .all(|stage| stage.status == ReadinessEvidenceStatus::NotObservable)
        {
            ReadinessSnapshotState::NotObservable
        } else {
            ReadinessSnapshotState::Missing
        };
    };
    if editor.profile().teaching_load_evidence == TeachingLoadEvidence::NotObservable
        && highest.rank() >= HarnessReadinessStage::Taught.rank()
        && highest.rank() < HarnessReadinessStage::Grounded.rank()
    {
        return ReadinessSnapshotState::NotObservable;
    }
    if highest.rank() >= HarnessReadinessStage::Grounded.rank() {
        ReadinessSnapshotState::Ready
    } else {
        ReadinessSnapshotState::Partial
    }
}

fn readiness_snapshot_from_evidence(
    editor: &Editor,
    mut evidence: Vec<HarnessReadinessEvidence>,
) -> ReadinessSnapshot {
    evidence.sort_by_key(|item| {
        (
            item.stage.rank(),
            evidence_source_rank(item.source),
            item.observed_at,
        )
    });
    let highest_ready_stage = highest_effective_stage(&evidence);
    let stages = HarnessReadinessStage::ALL
        .iter()
        .copied()
        .map(|stage| {
            let stage_evidence: Vec<_> = evidence
                .iter()
                .filter(|item| item.stage == stage)
                .cloned()
                .collect();
            ReadinessStageReport {
                stage,
                status: strongest_stage_status(
                    &stage_evidence,
                    default_missing_stage_status(editor, stage),
                ),
                evidence: stage_evidence,
            }
        })
        .collect::<Vec<_>>();
    ReadinessSnapshot {
        state: classify_readiness_snapshot(editor, &stages, highest_ready_stage),
        highest_ready_stage,
        stages,
        detail: None,
    }
}

fn unavailable_readiness_snapshot(editor: &Editor, detail: impl Into<String>) -> ReadinessSnapshot {
    let mut snapshot = readiness_snapshot_from_evidence(editor, Vec::new());
    snapshot.state = ReadinessSnapshotState::Unavailable;
    snapshot.detail = Some(detail.into());
    snapshot
}

fn failed_readiness_snapshot(editor: &Editor, detail: impl Into<String>) -> ReadinessSnapshot {
    let mut snapshot = readiness_snapshot_from_evidence(editor, Vec::new());
    snapshot.state = ReadinessSnapshotState::Failed;
    snapshot.detail = Some(detail.into());
    snapshot
}

fn current_versions(editor: &Editor) -> CurrentHarnessVersions<'static> {
    CurrentHarnessVersions {
        teaching_version: HARNESS_TEACHING_VERSION,
        managed_config_version: (editor.profile().mcp_support != McpTransportSupport::None)
            .then_some(MANAGED_CONFIG_VERSION),
        rules_hash: mcp_types::rules_hash::canonical_rules_hash(),
    }
}

fn local_readiness_snapshot(
    editor: &Editor,
    installation_id: Option<Uuid>,
    ledger: std::result::Result<Option<&HarnessReadinessLedger>, &str>,
) -> ReadinessSnapshot {
    match ledger {
        Ok(Some(ledger)) if Some(ledger.installation_id) != installation_id => {
            failed_readiness_snapshot(
                editor,
                "local readiness belongs to a different or missing installation identity",
            )
        }
        Ok(Some(ledger)) => readiness_snapshot_from_evidence(
            editor,
            effective_evidence_records_for(
                &ledger.evidence,
                editor.harness_id(),
                current_versions(editor),
            ),
        ),
        Ok(None) => readiness_snapshot_from_evidence(editor, Vec::new()),
        Err(detail) => failed_readiness_snapshot(editor, detail),
    }
}

fn remote_status_evidence(status: HarnessReadinessStatusResponse) -> Vec<HarnessReadinessEvidence> {
    status
        .evidence
        .into_iter()
        .map(|evidence| HarnessReadinessEvidence {
            schema_version: mcp_types::HARNESS_READINESS_SCHEMA_VERSION,
            harness_id: evidence.harness_id,
            stage: evidence.stage,
            status: evidence.status,
            source: evidence.source,
            observed_at: evidence.occurred_at,
            teaching_version: evidence.teaching_version,
            managed_config_version: evidence.managed_config_version,
            rules_hash: evidence.rules_hash,
        })
        .collect()
}

fn remote_readiness_snapshot(
    editor: &Editor,
    result: std::result::Result<HarnessReadinessStatusResponse, McpError>,
) -> ReadinessSnapshot {
    match result {
        Ok(status) => {
            let evidence = remote_status_evidence(status);
            readiness_snapshot_from_evidence(
                editor,
                effective_evidence_records_for(
                    &evidence,
                    editor.harness_id(),
                    current_versions(editor),
                ),
            )
        }
        Err(McpError::Http { status: 404, .. }) => unavailable_readiness_snapshot(
            editor,
            "the server readiness endpoint is not available on this deployment yet",
        ),
        Err(error) if error.is_retryable() => unavailable_readiness_snapshot(
            editor,
            "remote readiness is temporarily unavailable; local evidence remains available for diagnostics but cannot verify a server-observed connection",
        ),
        Err(_) => failed_readiness_snapshot(
            editor,
            "remote readiness response was rejected or failed identity validation",
        ),
    }
}

fn readiness_check(
    surface: &'static str,
    snapshot: &ReadinessSnapshot,
    stale_fix: Option<&str>,
) -> SurfaceCheck {
    let highest = snapshot
        .highest_ready_stage
        .map(|stage| format!("{stage:?}").to_ascii_lowercase());
    match snapshot.state {
        ReadinessSnapshotState::Ready => SurfaceCheck::pass(
            surface,
            format!(
                "current evidence reaches {}",
                highest.as_deref().unwrap_or("grounded")
            ),
            None,
        ),
        ReadinessSnapshotState::Partial => SurfaceCheck::warning(
            surface,
            format!(
                "current evidence reaches {}; run the harness and call init/context to prove use",
                highest.as_deref().unwrap_or("an early stage")
            ),
            None,
            None,
        ),
        ReadinessSnapshotState::Stale => SurfaceCheck::failure(
            surface,
            "evidence is stale for the current teaching/config/rules identity",
            None,
            stale_fix,
        ),
        ReadinessSnapshotState::Missing => SurfaceCheck::warning(
            surface,
            "no readiness evidence has been observed yet",
            None,
            None,
        ),
        ReadinessSnapshotState::NotObservable => SurfaceCheck::warning(
            surface,
            "this harness cannot directly prove that teaching entered context",
            None,
            None,
        ),
        ReadinessSnapshotState::Failed => SurfaceCheck::failure(
            surface,
            snapshot
                .detail
                .as_deref()
                .unwrap_or("readiness state failed validation"),
            None,
            stale_fix,
        ),
        ReadinessSnapshotState::Unavailable => SurfaceCheck::warning(
            surface,
            snapshot
                .detail
                .as_deref()
                .unwrap_or("readiness source is unavailable"),
            None,
            None,
        ),
    }
}

fn snapshot_has_verified_stage_from(
    snapshot: &ReadinessSnapshot,
    stage: HarnessReadinessStage,
    sources: &[ReadinessEvidenceSource],
) -> bool {
    snapshot.stages.iter().any(|stage_report| {
        stage_report.stage == stage
            && stage_report.evidence.iter().any(|evidence| {
                evidence.status == ReadinessEvidenceStatus::Verified
                    && sources.contains(&evidence.source)
            })
    })
}

fn activation_guidance_for_editor(
    report: &EditorReport,
    shared_failure: bool,
) -> EditorActivationGuidance {
    let editor = Editor::from_id(report.editor)
        .expect("doctor reports are constructed only from enabled editor identities");
    let has_editor_failure = report.checks.iter().any(|check| {
        check.status == CheckStatus::Fail
            && !matches!(check.surface, "readiness_local" | "readiness_remote")
    });
    let has_failed_readiness =
        matches!(report.readiness.local.state, ReadinessSnapshotState::Failed)
            || matches!(
                report.readiness.remote.state,
                ReadinessSnapshotState::Failed
            );

    if shared_failure || has_editor_failure || has_failed_readiness {
        let next_step = report
            .checks
            .iter()
            .find(|check| check.status == CheckStatus::Fail)
            .and_then(|check| check.fix.as_deref())
            .map(|fix| format!("{fix}; then rerun doctor."))
            .unwrap_or_else(|| {
                "Resolve the failing check shown above; if it has no fix, rerun setup for this editor. Then rerun doctor."
                    .to_string()
            });
        return EditorActivationGuidance {
            editor_name: report.editor_name,
            state: ActivationGuidanceState::RepairRequired,
            status: "managed setup is not healthy enough to verify activation".to_string(),
            next_step,
        };
    }

    if !editor.has_mcp_transport() {
        return EditorActivationGuidance {
            editor_name: report.editor_name,
            state: ActivationGuidanceState::RulesOnly,
            status: "rules are configured; this integration has no MCP transport or handshake"
                .to_string(),
            next_step: editor.activation_reload_instruction().to_string(),
        };
    }

    let remote = &report.readiness.remote;
    if snapshot_has_verified_stage_from(
        remote,
        HarnessReadinessStage::Grounded,
        &[
            ReadinessEvidenceSource::InitTool,
            ReadinessEvidenceSource::ContextTool,
        ],
    ) {
        return EditorActivationGuidance {
            editor_name: report.editor_name,
            state: ActivationGuidanceState::GroundingObserved,
            status: "the server observed a version-current init/context grounding call for this installation and harness; checkout identity and answer quality are not inferred"
                .to_string(),
            next_step:
                "Confirm the exact checkout binding and index status in the checkout-specific steps below."
                    .to_string(),
        };
    }

    if snapshot_has_verified_stage_from(
        remote,
        HarnessReadinessStage::Connected,
        &[ReadinessEvidenceSource::McpProtocolRequest],
    ) {
        return EditorActivationGuidance {
            editor_name: report.editor_name,
            state: ActivationGuidanceState::ConnectionObserved,
            status: "the server observed a real MCP protocol request for this installation and harness; exact checkout identity and grounding are still pending"
                .to_string(),
            next_step:
                "Confirm the exact checkout binding and index status in the checkout-specific steps below."
                    .to_string(),
        };
    }

    if remote.state == ReadinessSnapshotState::Unavailable {
        return EditorActivationGuidance {
            editor_name: report.editor_name,
            state: ActivationGuidanceState::ConnectionUnverified,
            status:
                "remote readiness is unavailable, so this doctor run cannot verify a connection"
                    .to_string(),
            next_step: format!(
                "{} Then rerun doctor; do not switch away from hosted MCP.",
                editor.activation_reload_instruction()
            ),
        };
    }

    EditorActivationGuidance {
        editor_name: report.editor_name,
        state: ActivationGuidanceState::RestartRequired,
        status: "no server-observed MCP handshake exists for the current managed identity"
            .to_string(),
        next_step: editor.activation_reload_instruction().to_string(),
    }
}

fn has_current_checkout_binding(project_path: Option<&Path>) -> bool {
    let Some(project_path) = project_path else {
        return false;
    };
    let Ok(Some(config)) = super::read_project_config(project_path) else {
        return false;
    };
    let valid_workspace = config
        .workspace_id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_some_and(|value| !value.is_nil());
    let valid_project = config
        .project_id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_some_and(|value| !value.is_nil());

    valid_workspace
        && valid_project
        && super::checkout_root_matches(config.checkout_root.as_deref(), project_path)
}

fn activation_guidance_lines(report: &DoctorReport, project_path: Option<&Path>) -> Vec<String> {
    if report.editors.is_empty() {
        return vec![
            "No coding harness is selected, so there is no runtime connection to verify."
                .to_string(),
            "Run contextstream-mcp setup --editors <editor-id> --project-path /path/to/project."
                .to_string(),
        ];
    }

    let shared_failure = report.installation.status == CheckStatus::Fail
        || report.credentials.status == CheckStatus::Fail;
    let mut lines = Vec::new();
    let mut has_mcp_editor = false;
    for editor_report in &report.editors {
        let guidance = activation_guidance_for_editor(editor_report, shared_failure);
        has_mcp_editor |=
            Editor::from_id(editor_report.editor).is_some_and(|editor| editor.has_mcp_transport());
        let marker = match guidance.state {
            ActivationGuidanceState::RepairRequired => "✗",
            ActivationGuidanceState::RulesOnly => "·",
            ActivationGuidanceState::RestartRequired => "○",
            ActivationGuidanceState::ConnectionUnverified => "⚠",
            ActivationGuidanceState::ConnectionObserved
            | ActivationGuidanceState::GroundingObserved => "✓",
        };
        lines.push(format!(
            "{marker} {}: {}.",
            guidance.editor_name, guidance.status
        ));
        lines.push(format!("  → {}", guidance.next_step));
    }

    if !has_mcp_editor {
        return lines;
    }

    let editor_ids = report.targeting.editors.join(",");
    if has_current_checkout_binding(project_path) {
        lines.push(
            "Before judging repository answers, ask the editor to run project(action=\"index_status\") for this exact checkout."
                .to_string(),
        );
        lines.push(
            "If it reports checkout_index_unconfirmed, checkout_not_registered, bridge_offline, or requires_sync_bridge, keep hosted MCP configured and repair the managed sync path."
                .to_string(),
        );
        lines.push(format!(
            "  → contextstream-mcp doctor --repair --scope global --editors {editor_ids}"
        ));
        lines.push(format!(
            "After index_status confirms this exact checkout is ready, first-value prompt: {}",
            super::first_value_prompt()
        ));
    } else {
        lines.push(
            "No exact checkout binding was confirmed, so repository-grounded first value is still pending."
                .to_string(),
        );
        lines.push(format!(
            "  → From the intended checkout, run contextstream-mcp setup --project-path . --editors {editor_ids}"
        ));
    }

    lines
}

fn print_activation_guidance(report: &DoctorReport, project_path: Option<&Path>) {
    println!();
    println!("{}", style("Activation path").bold());
    for line in activation_guidance_lines(report, project_path) {
        println!("  {line}");
    }
}

fn support_command(report: &DoctorReport) -> String {
    let targets = if report.targeting.editors.is_empty() {
        "--only-configured".to_string()
    } else {
        format!("--editors {}", report.targeting.editors.join(","))
    };
    format!(
        "contextstream-mcp doctor --support --scope {} {targets}",
        report.targeting.scope.as_cli_value()
    )
}

fn current_transport_state() -> DoctorTransportState {
    match super::read_setup_transport_marker_result() {
        Ok(Some(super::SetupTransportPreference::HostedRemote)) => DoctorTransportState::Hosted,
        Ok(Some(super::SetupTransportPreference::LocalBinary)) => DoctorTransportState::Local,
        Ok(None) => DoctorTransportState::LegacyUnknown,
        Err(_) => DoctorTransportState::Unreadable,
    }
}

fn doctor_support_project(project_path: Option<&Path>) -> DoctorSupportProject {
    let Some(project_path) = project_path else {
        return DoctorSupportProject::with_state(DoctorSupportProjectState::NotChecked);
    };
    let config = match super::read_project_config(project_path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            return DoctorSupportProject::with_state(DoctorSupportProjectState::Missing);
        }
        Err(_) => {
            return DoctorSupportProject::with_state(DoctorSupportProjectState::Unreadable);
        }
    };
    let workspace_id = config
        .workspace_id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|value| !value.is_nil());
    let project_id = config
        .project_id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|value| !value.is_nil());
    let (Some(workspace_id), Some(project_id)) = (workspace_id, project_id) else {
        return DoctorSupportProject::with_state(DoctorSupportProjectState::BindingMismatch);
    };
    if !super::checkout_root_matches(config.checkout_root.as_deref(), project_path) {
        return DoctorSupportProject {
            state: DoctorSupportProjectState::BindingMismatch,
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
            checkout_id: None,
            checkout_locator: None,
            repository_fingerprint: None,
        };
    }

    let mut support = DoctorSupportProject {
        state: DoctorSupportProjectState::Bound,
        workspace_id: Some(workspace_id),
        project_id: Some(project_id),
        checkout_id: None,
        checkout_locator: None,
        repository_fingerprint: None,
    };
    if let Ok(binding) = mcp_session::checkout_identity::validate_checkout_binding(
        project_path,
        Some(workspace_id),
        project_id,
    ) {
        support.state = DoctorSupportProjectState::ExactCheckout;
        support.checkout_id = Some(binding.checkout_id.to_string());
        support.repository_fingerprint = Some(binding.repository_fingerprint.to_string());
        support.checkout_locator = binding
            .checkout_root
            .to_str()
            .and_then(ContextStreamClient::checkout_routing_scope)
            .map(|scope| scope.checkout_locator);
    }
    support
}

fn doctor_support_bridge(
    report: &DoctorReport,
    transport: DoctorTransportState,
) -> DoctorSupportBridge {
    let required = transport == DoctorTransportState::Hosted
        && report.editors.iter().any(|editor| {
            Editor::from_id(editor.editor).is_some_and(|editor| editor.has_mcp_transport())
        });
    if !required {
        return DoctorSupportBridge {
            required: false,
            registration_readable: true,
            registration_state: None,
            activation_state: None,
            platform: None,
            health_state: None,
            heartbeat_fresh: None,
            target_count: None,
            version: None,
        };
    }

    let registration = super::sync_bridge_registration_status();
    let health = crate::watch::sync_bridge_health();
    DoctorSupportBridge {
        required: true,
        registration_readable: registration.is_ok(),
        registration_state: registration.as_ref().ok().map(|value| value.state),
        activation_state: registration.as_ref().ok().map(|value| value.activation),
        platform: registration.as_ref().ok().map(|value| value.platform),
        health_state: Some(health.state),
        heartbeat_fresh: Some(health.heartbeat_fresh),
        target_count: Some(health.target_count),
        version: health.version,
    }
}

fn doctor_support_report(
    report: &DoctorReport,
    project_path: Option<&Path>,
) -> DoctorSupportReport {
    let transport = current_transport_state();
    let installation_id = mcp_client::activation::existing_installation_id()
        .ok()
        .flatten();
    let project = doctor_support_project(project_path);
    let sync_bridge = doctor_support_bridge(report, transport);
    doctor_support_report_from_parts(report, transport, installation_id, project, sync_bridge)
}

fn doctor_support_report_from_parts(
    report: &DoctorReport,
    transport: DoctorTransportState,
    installation_id: Option<Uuid>,
    project: DoctorSupportProject,
    sync_bridge: DoctorSupportBridge,
) -> DoctorSupportReport {
    let shared_failure = report.installation.status == CheckStatus::Fail
        || report.credentials.status == CheckStatus::Fail;
    let editors = report
        .editors
        .iter()
        .map(|editor| {
            let guidance = activation_guidance_for_editor(editor, shared_failure);
            let recommended_action = match guidance.state {
                ActivationGuidanceState::RepairRequired => {
                    DoctorSupportRecommendedAction::RepairManagedSurfaces
                }
                ActivationGuidanceState::RulesOnly => DoctorSupportRecommendedAction::ReloadRules,
                ActivationGuidanceState::RestartRequired => {
                    DoctorSupportRecommendedAction::ReloadEditor
                }
                ActivationGuidanceState::ConnectionUnverified => {
                    DoctorSupportRecommendedAction::ReloadAndRetryDoctor
                }
                ActivationGuidanceState::ConnectionObserved
                | ActivationGuidanceState::GroundingObserved => {
                    DoctorSupportRecommendedAction::VerifyCheckoutIndex
                }
            };
            let repair_command =
                (guidance.state == ActivationGuidanceState::RepairRequired).then(|| {
                    format!(
                        "contextstream-mcp doctor --repair --scope {} --editors {}",
                        report.targeting.scope.as_cli_value(),
                        editor.editor
                    )
                });
            DoctorSupportEditor {
                editor: editor.editor,
                activation_state: guidance.state,
                recommended_action,
                remote_readiness: editor.readiness.remote.state,
                highest_server_stage: editor.readiness.remote.highest_ready_stage,
                repair_command,
            }
        })
        .collect();

    DoctorSupportReport {
        schema_version: DOCTOR_SUPPORT_REPORT_SCHEMA_VERSION,
        source_report_schema_version: report.schema_version,
        mcp_version: env!("CARGO_PKG_VERSION"),
        transport: transport.as_support_value(),
        installation_id,
        targeting: DoctorSupportTargeting {
            scope: report.targeting.scope,
            source: report.targeting.source,
            editors: report.targeting.editors.clone(),
            detection_fallback_allowed: report.targeting.detection_fallback_allowed,
        },
        editors,
        project,
        sync_bridge,
        summary: DoctorSupportSummary {
            installation: report.installation.status,
            credentials: report.credentials.status,
            pass: report.pass,
            warn: report.warn,
            fail: report.fail,
            skipped: report.skipped,
            setup_healthy: !report.has_setup_failures(),
            doctor_has_failures: report.has_failures(),
        },
    }
}

fn sync_bridge_check(
    editor: &Editor,
    transport: DoctorTransportState,
    registration: Option<&Result<super::SyncBridgeServiceRegistration>>,
    health: Option<&crate::watch::SyncBridgeHealth>,
) -> Option<SurfaceCheck> {
    if !editor.has_mcp_transport() {
        return None;
    }
    const SURFACE: &str = "sync_bridge";
    let fix = format!(
        "contextstream-mcp doctor --repair --scope global --editors {}",
        editor.id()
    );
    match transport {
        DoctorTransportState::Local => Some(SurfaceCheck::skipped(
            SURFACE,
            "local MCP recovery mode does not use the hosted sync bridge",
        )),
        DoctorTransportState::LegacyUnknown => Some(SurfaceCheck::warning(
            SURFACE,
            "transport selection predates managed bridge registration; rerun setup to keep hosted indexing fresh after login",
            None,
            Some("contextstream-mcp setup"),
        )),
        DoctorTransportState::Unreadable => Some(SurfaceCheck::failure(
            SURFACE,
            "transport selection is unreadable; bridge requirements cannot be validated",
            None,
            None,
        )),
        DoctorTransportState::Hosted => {
            if health.is_some_and(|health| {
                health.state == crate::watch::SyncBridgeHealthState::Disabled
            }) {
                return Some(SurfaceCheck::warning(
                    SURFACE,
                    "hosted sync bridge is explicitly disabled; local checkout edits require hooks or ContextStream Desktop",
                    None,
                    None,
                ));
            }
            let Some(registration) = registration else {
                return Some(SurfaceCheck::failure(
                    SURFACE,
                    "hosted sync bridge registration was not checked",
                    None,
                    Some(&fix),
                ));
            };
            let registration = match registration {
                Ok(registration) => registration,
                Err(_) => {
                    return Some(SurfaceCheck::failure(
                        SURFACE,
                        "hosted sync bridge registration is unreadable",
                        None,
                        Some(&fix),
                    ))
                }
            };
            match registration.state {
                super::SyncBridgeRegistrationState::Missing => {
                    return Some(SurfaceCheck::failure(
                        SURFACE,
                        "hosted sync bridge is not registered for machine startup",
                        None,
                        Some(&fix),
                    ))
                }
                super::SyncBridgeRegistrationState::Conflict => {
                    return Some(SurfaceCheck::failure(
                        SURFACE,
                        "the startup path is occupied by a non-matching file and was preserved",
                        None,
                        None,
                    ))
                }
                super::SyncBridgeRegistrationState::Unsupported => {
                    return Some(SurfaceCheck::warning(
                        SURFACE,
                        "automatic startup registration is not supported on this platform; setup still launches the bridge for the current login",
                        None,
                        None,
                    ))
                }
                super::SyncBridgeRegistrationState::Registered => {}
            }

            let Some(health) = health else {
                return Some(SurfaceCheck::failure(
                    SURFACE,
                    "hosted sync bridge health was not checked",
                    None,
                    Some(&fix),
                ));
            };
            match health.state {
                crate::watch::SyncBridgeHealthState::Running
                    if health.version.as_deref() != Some(mcp_types::config::VERSION) =>
                {
                    Some(SurfaceCheck::failure(
                        SURFACE,
                        format!(
                            "hosted sync bridge is running version {}, but this installer requires {}",
                            health.version.as_deref().unwrap_or("unknown"),
                            mcp_types::config::VERSION
                        ),
                        None,
                        Some(&fix),
                    ))
                }
                crate::watch::SyncBridgeHealthState::Running => {
                    Some(SurfaceCheck::pass(
                        SURFACE,
                        format!(
                            "hosted sync bridge is running and monitoring {} checkout root(s)",
                            health.target_count
                        ),
                        None,
                    ))
                }
                crate::watch::SyncBridgeHealthState::Disabled => unreachable!(
                    "disabled bridge health is handled before registration validation"
                ),
                crate::watch::SyncBridgeHealthState::Stopped => Some(SurfaceCheck::failure(
                    SURFACE,
                    "hosted sync bridge is registered but not running",
                    None,
                    Some(&fix),
                )),
                crate::watch::SyncBridgeHealthState::Degraded => Some(SurfaceCheck::failure(
                    SURFACE,
                    "hosted sync bridge heartbeat is stale, invalid, or inconsistent with its singleton lock",
                    None,
                    Some(&fix),
                )),
            }
        }
    }
}

fn doctor_client_config() -> Result<Config> {
    let saved = super::read_saved_credentials()?;
    let nonempty_env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let api_key = nonempty_env("CONTEXTSTREAM_API_KEY").or(saved.api_key);
    let jwt = nonempty_env("CONTEXTSTREAM_JWT").or(saved.jwt);
    if api_key.is_none() && jwt.is_none() {
        anyhow::bail!("no saved credentials");
    }
    let api_url = nonempty_env("CONTEXTSTREAM_API_URL")
        .or(saved.api_url)
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    Ok(Config {
        api_url,
        api_key,
        jwt,
        is_http_transport: false,
        ..Default::default()
    })
}

struct CredentialProbe {
    check: SurfaceCheck,
    client: Option<ContextStreamClient>,
}

async fn probe_credentials_client(
    client: ContextStreamClient,
    timeout: std::time::Duration,
) -> CredentialProbe {
    const SURFACE: &str = "credentials";
    const FIX: &str = "contextstream-mcp setup";

    match tokio::time::timeout(timeout, client.me()).await {
        Ok(Ok(_)) => CredentialProbe {
            check: SurfaceCheck::pass(SURFACE, "valid", None),
            client: Some(client),
        },
        Ok(Err(error)) if error.is_retryable() => {
            tracing::debug!(%error, "doctor credential validation temporarily unavailable");
            CredentialProbe {
                check: SurfaceCheck::warning(
                    SURFACE,
                    "validation unavailable (offline or transient server error)",
                    None,
                    None,
                ),
                client: None,
            }
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "doctor credential validation failed");
            CredentialProbe {
                check: SurfaceCheck::problem(
                    SURFACE,
                    CheckStatus::Fail,
                    "credentials were rejected",
                    None,
                    FIX,
                ),
                client: None,
            }
        }
        Err(_) => CredentialProbe {
            check: SurfaceCheck::warning(
                SURFACE,
                "validation timed out (offline?) — filesystem and local readiness checks still apply",
                None,
                None,
            ),
            client: None,
        },
    }
}

async fn check_credentials() -> CredentialProbe {
    const SURFACE: &str = "credentials";
    const FIX: &str = "contextstream-mcp setup";

    let config = match doctor_client_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::debug!(%error, "doctor could not load credentials");
            return CredentialProbe {
                check: SurfaceCheck::problem(
                    SURFACE,
                    CheckStatus::Fail,
                    "credentials are missing or unreadable",
                    None,
                    FIX,
                ),
                client: None,
            };
        }
    };
    let client = ContextStreamClient::new(config);
    probe_credentials_client(client, std::time::Duration::from_secs(10)).await
}

fn resolve_doctor_targets(options: &DoctorOptions) -> Result<(Vec<Editor>, &'static str, bool)> {
    let detection_fallback_allowed =
        options.explicit_editors.is_none() && !options.only_configured && !options.repair;
    let suppress_detection = options.only_configured || options.repair;
    let (targets, source) = super::resolve_hook_refresh_editors(
        options.explicit_editors.as_deref(),
        suppress_detection,
    )?;
    if options.repair && targets.is_empty() {
        anyhow::bail!(
            "Doctor repair has no authorized targets. Run setup first or pass --editors explicitly; no editor files were changed."
        );
    }
    Ok((targets, source, detection_fallback_allowed))
}

fn validate_repair_scope(options: &DoctorOptions) -> Result<()> {
    if !options.scope_was_explicit {
        anyhow::bail!(
            "Doctor repair requires an explicit --scope global|project|all; no files were changed."
        );
    }
    if !options.scope.include_project() {
        return Ok(());
    }

    let project_path = options.project_path.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Doctor could not resolve a project directory; project repair changed no files."
        )
    })?;
    let cwd = std::env::current_dir().context(
        "Doctor could not resolve the working directory; project repair changed no files",
    )?;
    let canonical_project = project_path.canonicalize().with_context(|| {
        format!(
            "Doctor could not resolve project directory {}; project repair changed no files",
            project_path.display()
        )
    })?;
    let canonical_cwd = cwd.canonicalize().with_context(|| {
        "Doctor could not resolve the working directory; project repair changed no files"
    })?;
    if canonical_project != canonical_cwd {
        anyhow::bail!(
            "Doctor project repair target {} is not the current working directory {}; no files were changed.",
            canonical_project.display(),
            canonical_cwd.display()
        );
    }
    let is_root = canonical_project.parent().is_none();
    let is_home = dirs::home_dir()
        .and_then(|home| home.canonicalize().ok())
        .is_some_and(|home| home == canonical_project);
    if is_root || is_home {
        anyhow::bail!(
            "Doctor refuses project repair in {}; run it from the intended project directory or use --scope global. No files were changed.",
            canonical_project.display()
        );
    }
    Ok(())
}

async fn run_repairs(options: &DoctorOptions, targets: &[Editor]) -> DoctorRepairReport {
    let scope = options.scope.as_cli_value();
    let mut operations = vec!["configs", "rules"];
    let mut failures = Vec::new();
    if let Err(error) =
        super::update_configs_scoped_noninteractive(scope, Some(targets), true).await
    {
        failures.push(format!("configs: {error}"));
    }
    if let Err(error) = super::update_rules_scoped(scope, None, None, Some(targets), true).await {
        failures.push(format!("rules: {error}"));
    }
    if options.scope.include_global() {
        operations.push("hooks");
        if let Err(error) = super::update_hooks_scoped("global", Some(targets), true).await {
            failures.push(format!("hooks: {error}"));
        }
    }
    let changes = if options.dry_run {
        super::safe_edit::take_planned_changes()
            .into_iter()
            .map(|change| RepairChange {
                path: change.path.display().to_string(),
                action: change.action.label(),
            })
            .collect()
    } else {
        Vec::new()
    };
    DoctorRepairReport {
        mode: if options.dry_run {
            "dry_run"
        } else {
            "applied"
        },
        operations,
        changes,
        failures,
    }
}

async fn build_report_for_targets(
    options: &DoctorOptions,
    targets: &[Editor],
    source: &'static str,
    detection_fallback_allowed: bool,
    repair: Option<DoctorRepairReport>,
) -> DoctorReport {
    let bridge_transport = current_transport_state();
    let needs_bridge_check = bridge_transport == DoctorTransportState::Hosted
        && targets.iter().any(Editor::has_mcp_transport);
    let bridge_registration = needs_bridge_check.then(super::sync_bridge_registration_status);
    let bridge_health = needs_bridge_check.then(crate::watch::sync_bridge_health);

    let installation_result = mcp_client::activation::existing_installation_id();
    let installation_id = installation_result.as_ref().ok().copied().flatten();
    let installation = match &installation_result {
        Ok(Some(_)) => SurfaceCheck::pass(
            "installation",
            "managed installation identity is valid",
            None,
        ),
        Ok(None) => SurfaceCheck::failure(
            "installation",
            "managed installation identity is missing",
            None,
            Some("contextstream-mcp setup"),
        ),
        Err(error) => {
            tracing::debug!(%error, "doctor could not read installation identity");
            SurfaceCheck::failure(
                "installation",
                "managed installation identity is unreadable; state was preserved unchanged",
                None,
                None,
            )
        }
    };
    let ledger_result = read_harness_readiness();
    let credential_probe = check_credentials().await;
    let remote_results = if let (Some(client), Some(installation_id)) =
        (credential_probe.client.as_ref(), installation_id)
    {
        join_all(targets.iter().map(|editor| {
            client.harness_readiness_status_for_installation(installation_id, editor.harness_id())
        }))
        .await
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>()
    } else {
        (0..targets.len()).map(|_| None).collect()
    };

    let editor_reports: Vec<EditorReport> = targets
        .iter()
        .zip(remote_results)
        .map(|(editor, remote_result)| {
            let mut checks = check_editor_surfaces(
                editor,
                options.project_path.as_deref(),
                options.scope,
                installation_id,
            );
            let local = match (&installation_result, &ledger_result) {
                (Err(_), _) => failed_readiness_snapshot(
                    editor,
                    "installation identity is unreadable; readiness state was preserved unchanged",
                ),
                (_, Ok(ledger)) => {
                    local_readiness_snapshot(editor, installation_id, Ok(ledger.as_ref()))
                }
                (_, Err(_)) => local_readiness_snapshot(
                    editor,
                    installation_id,
                    Err("local readiness ledger is unreadable; it was preserved unchanged"),
                ),
            };
            let remote = match remote_result {
                Some(result) => remote_readiness_snapshot(editor, result),
                None if installation_id.is_none() => unavailable_readiness_snapshot(
                    editor,
                    "no existing installation identity is available for a remote query",
                ),
                None => unavailable_readiness_snapshot(
                    editor,
                    "remote readiness was skipped because credentials could not be validated",
                ),
            };
            let repair_fix = format!(
                "contextstream-mcp doctor --repair --scope {} --editors {}",
                options.scope.as_cli_value(),
                editor.id()
            );
            checks.push(readiness_check(
                "readiness_local",
                &local,
                Some(&repair_fix),
            ));
            checks.push(readiness_check("readiness_remote", &remote, None));
            if let Some(check) = sync_bridge_check(
                editor,
                bridge_transport,
                bridge_registration.as_ref(),
                bridge_health.as_ref(),
            ) {
                checks.push(check);
            }
            EditorReport {
                editor: editor.id(),
                editor_name: editor.display_name(),
                checks,
                readiness: HarnessReadinessReport {
                    harness_id: editor.harness_id(),
                    teaching_load_evidence: editor.profile().teaching_load_evidence,
                    local,
                    remote,
                },
            }
        })
        .collect();
    let credentials = credential_probe.check;

    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    let mut skipped = 0;
    for check in editor_reports
        .iter()
        .flat_map(|report| report.checks.iter())
        .chain(std::iter::once(&installation))
        .chain(std::iter::once(&credentials))
    {
        match check.status {
            CheckStatus::Pass => pass += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Fail => fail += 1,
            CheckStatus::Skipped => skipped += 1,
        }
    }

    DoctorReport {
        schema_version: DOCTOR_REPORT_SCHEMA_VERSION,
        targeting: DoctorTargeting {
            scope: options.scope,
            source,
            editors: targets.iter().map(Editor::id).collect(),
            detection_fallback_allowed,
        },
        editors: editor_reports,
        installation,
        credentials,
        repair,
        pass,
        warn,
        fail,
        skipped,
    }
}

/// Build a post-setup report for the editors that setup just configured.
///
/// The explicit target list is important: setup completion must never broaden
/// its diagnostic scope by detecting another editor on the machine.
pub async fn build_report(project_path: Option<&Path>, targets: &[Editor]) -> DoctorReport {
    let mut options = DoctorOptions::new(project_path.map(Path::to_path_buf));
    options.scope = if project_path.is_some() {
        DoctorScope::All
    } else {
        DoctorScope::Global
    };
    options.explicit_editors = Some(targets.to_vec());
    options.only_configured = true;
    build_report_for_targets(&options, targets, "configured by setup", false, None).await
}

fn status_glyph(status: CheckStatus) -> console::StyledObject<&'static str> {
    match status {
        CheckStatus::Pass => style("✓").green(),
        CheckStatus::Warn => style("⚠").yellow(),
        CheckStatus::Fail => style("✗").red(),
        CheckStatus::Skipped => style("·").dim(),
    }
}

fn print_check(check: &SurfaceCheck, indent: &str) {
    let mut line = format!(
        "{}{} {:<13} {}",
        indent,
        status_glyph(check.status),
        check.surface.replace('_', " "),
        check.detail
    );
    if let Some(path) = &check.path {
        line.push_str(&format!("  {}", style(path).dim()));
    }
    println!("{}", line);
    if let Some(fix) = &check.fix {
        println!("{}    → {}", indent, style(fix).cyan());
    }
}

struct DoctorDryRunReset;

impl Drop for DoctorDryRunReset {
    fn drop(&mut self) {
        super::safe_edit::set_dry_run(false);
        let _ = super::safe_edit::take_planned_changes();
    }
}

/// Run doctor and print a human, complete local JSON, or privacy-bounded
/// support report.
/// Returns whether any check failed so main can set the exit code.
pub async fn run_doctor(options: DoctorOptions, json: bool, support: bool) -> Result<bool> {
    if options.dry_run && !options.repair {
        anyhow::bail!("--dry-run requires --repair; no files were changed");
    }
    if json && support {
        anyhow::bail!("--json and --support are mutually exclusive");
    }

    let (targets, source, detection_fallback_allowed) = resolve_doctor_targets(&options)?;
    if options.repair {
        validate_repair_scope(&options)?;
        mcp_client::activation::existing_installation_id().context(
            "installation identity is unreadable; repair was refused before any editor file changed",
        )?;
        super::read_setup_transport_marker_result().context(
            "transport state is unreadable; repair was refused before any editor file changed",
        )?;
        if super::get_api_key_result()
            .context(
                "credentials are unreadable; repair was refused before any editor file changed",
            )?
            .is_none()
        {
            anyhow::bail!(
                "No API key found. Run 'contextstream-mcp setup' first; repair changed no editor files."
            );
        }
    }
    super::safe_edit::set_dry_run(options.dry_run);
    let _dry_run_reset = DoctorDryRunReset;
    if options.dry_run {
        let _ = super::safe_edit::take_planned_changes();
    }
    let repair = if options.repair {
        Some(run_repairs(&options, &targets).await)
    } else {
        None
    };
    let report = build_report_for_targets(
        &options,
        &targets,
        source,
        detection_fallback_allowed,
        repair,
    )
    .await;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(report.has_failures());
    }
    if support {
        let support_report = doctor_support_report(&report, options.project_path.as_deref());
        println!("{}", serde_json::to_string_pretty(&support_report)?);
        return Ok(report.has_failures());
    }

    println!("{}", style("ContextStream Doctor").bold());
    let target_names = if report.targeting.editors.is_empty() {
        "none".to_string()
    } else {
        report.targeting.editors.join(", ")
    };
    println!(
        "Scope: {} · targets: {} ({})",
        report.targeting.scope.as_cli_value(),
        target_names,
        report.targeting.source
    );
    println!();
    print_check(&report.installation, "");
    print_check(&report.credentials, "");
    println!();
    for editor_report in &report.editors {
        println!("{}", style(editor_report.editor_name).bold());
        for check in &editor_report.checks {
            print_check(check, "  ");
        }
        println!();
    }
    if let Some(repair) = &report.repair {
        println!(
            "{}: {}",
            style("Repair").bold(),
            if repair.mode == "dry_run" {
                "preview only"
            } else {
                "applied"
            }
        );
        if repair.changes.is_empty() && repair.failures.is_empty() {
            println!(
                "  {} No managed changes were needed.",
                status_glyph(CheckStatus::Pass)
            );
        }
        for change in &repair.changes {
            println!("  {} {} {}", style("→").cyan(), change.action, change.path);
        }
        for failure in &repair.failures {
            println!("  {} {}", status_glyph(CheckStatus::Fail), failure);
        }
        println!();
    }
    println!(
        "Summary: {} pass, {} warn, {} fail, {} skipped",
        style(report.pass).green(),
        style(report.warn).yellow(),
        style(report.fail).red(),
        style(report.skipped).dim()
    );
    if report.has_failures() {
        println!(
            "{}",
            style("Fix commands are listed under each failing check; re-run doctor to verify.")
                .dim()
        );
    }
    print_activation_guidance(&report, options.project_path.as_deref());
    println!();
    println!(
        "{} {}",
        style("Shareable support report (no credentials or local paths):").dim(),
        style(support_command(&report)).cyan()
    );

    Ok(report.has_failures())
}

/// Compact post-setup health summary: prints only problems (or one green
/// line), no network. Used at the end of the setup wizard.
pub fn print_setup_health_summary(configured: &[Editor], project_path: Option<&Path>) {
    let mut problems = Vec::new();
    for editor in configured {
        let report = check_editor(editor, project_path);
        for check in report.checks {
            if matches!(check.status, CheckStatus::Fail) {
                problems.push((report.editor_name, check));
            }
        }
    }

    if problems.is_empty() {
        println!(
            "  {} All configured editor surfaces verified. Re-check anytime: {}",
            style("✓").green(),
            style("contextstream-mcp doctor").cyan()
        );
        return;
    }

    println!(
        "  {} {} surface(s) still need attention:",
        style("⚠").yellow(),
        problems.len()
    );
    for (editor_name, check) in problems {
        println!(
            "    {} {} {}: {}",
            style("✗").red(),
            editor_name,
            check.surface.replace('_', " "),
            check.detail
        );
        if let Some(fix) = check.fix {
            println!("      → {}", style(fix).cyan());
        }
    }
}

/// Print the compact setup summary from the exact report used to classify the
/// setup outcome. This keeps terminal claims and completion telemetry on one
/// evidence set and includes top-level installation/credential failures that
/// the legacy editor-only summary could not show.
pub fn print_setup_health_report(report: &DoctorReport) {
    let mut problems: Vec<(&str, &SurfaceCheck)> = Vec::new();
    if matches!(report.installation.status, CheckStatus::Fail) {
        problems.push(("Installation", &report.installation));
    }
    if matches!(report.credentials.status, CheckStatus::Fail) {
        problems.push(("Credentials", &report.credentials));
    }
    for editor in &report.editors {
        for check in &editor.checks {
            if matches!(check.status, CheckStatus::Fail)
                && !matches!(check.surface, "readiness_local" | "readiness_remote")
            {
                problems.push((editor.editor_name, check));
            }
        }
    }

    if problems.is_empty() {
        println!(
            "  {} Required setup surfaces verified. Runtime connection is verified separately after your editor starts.",
            style("✓").green()
        );
        return;
    }

    println!(
        "  {} {} required surface(s) still need attention:",
        style("⚠").yellow(),
        problems.len()
    );
    for (owner, check) in problems {
        println!(
            "    {} {} {}: {}",
            style("✗").red(),
            owner,
            check.surface.replace('_', " "),
            check.detail
        );
        if let Some(fix) = &check.fix {
            println!("      → {}", style(fix).cyan());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bridge_health(state: crate::watch::SyncBridgeHealthState) -> crate::watch::SyncBridgeHealth {
        crate::watch::SyncBridgeHealth {
            state,
            enabled: true,
            lock_held: Some(state == crate::watch::SyncBridgeHealthState::Running),
            heartbeat_fresh: state == crate::watch::SyncBridgeHealthState::Running,
            pid: Some(42),
            target_count: 2,
            refreshed_at: Some("2026-07-29T00:00:00Z".to_string()),
            role: "hosted_sync_bridge",
            version: Some(mcp_types::config::VERSION.to_string()),
            detail: "test".to_string(),
        }
    }

    #[test]
    fn every_hosted_mcp_harness_requires_registered_healthy_bridge() {
        let registered = Ok(super::super::SyncBridgeServiceRegistration {
            state: super::super::SyncBridgeRegistrationState::Registered,
            activation: super::super::SyncBridgeActivationState::Active,
            platform: "test",
            changed: false,
        });
        let running = bridge_health(crate::watch::SyncBridgeHealthState::Running);
        let healthy = sync_bridge_check(
            &Editor::Codex,
            DoctorTransportState::Hosted,
            Some(&registered),
            Some(&running),
        )
        .expect("check");
        assert_eq!(healthy.status, CheckStatus::Pass);
        assert!(healthy.detail.contains("2 checkout root"));

        let hook_capable = sync_bridge_check(
            &Editor::ClaudeCode,
            DoctorTransportState::Hosted,
            Some(&registered),
            Some(&running),
        )
        .expect("hook-capable hosted editors still depend on the sync bridge");
        assert_eq!(hook_capable.status, CheckStatus::Pass);

        let missing = Ok(super::super::SyncBridgeServiceRegistration {
            state: super::super::SyncBridgeRegistrationState::Missing,
            activation: super::super::SyncBridgeActivationState::Deferred,
            platform: "test",
            changed: false,
        });
        let absent = sync_bridge_check(
            &Editor::Codex,
            DoctorTransportState::Hosted,
            Some(&missing),
            Some(&running),
        )
        .expect("check");
        assert_eq!(absent.status, CheckStatus::Fail);

        let stopped = bridge_health(crate::watch::SyncBridgeHealthState::Stopped);
        let inactive = sync_bridge_check(
            &Editor::Codex,
            DoctorTransportState::Hosted,
            Some(&registered),
            Some(&stopped),
        )
        .expect("check");
        assert_eq!(inactive.status, CheckStatus::Fail);

        let mut stale_version = running;
        stale_version.version = Some("0.0.0-stale".to_string());
        let stale = sync_bridge_check(
            &Editor::Codex,
            DoctorTransportState::Hosted,
            Some(&registered),
            Some(&stale_version),
        )
        .expect("check");
        assert_eq!(stale.status, CheckStatus::Fail);
        assert!(stale.detail.contains("0.0.0-stale"));
    }

    #[test]
    fn bridge_check_respects_transport_and_mcp_capability() {
        assert!(
            sync_bridge_check(&Editor::Aider, DoctorTransportState::Hosted, None, None,).is_none()
        );
        assert_eq!(
            sync_bridge_check(&Editor::Codex, DoctorTransportState::Local, None, None,)
                .unwrap()
                .status,
            CheckStatus::Skipped
        );
        assert_eq!(
            sync_bridge_check(
                &Editor::Codex,
                DoctorTransportState::LegacyUnknown,
                None,
                None,
            )
            .unwrap()
            .status,
            CheckStatus::Warn
        );
        assert_eq!(
            sync_bridge_check(
                &Editor::Codex,
                DoctorTransportState::Hosted,
                None,
                Some(&bridge_health(
                    crate::watch::SyncBridgeHealthState::Disabled
                )),
            )
            .unwrap()
            .status,
            CheckStatus::Warn
        );
    }

    #[test]
    fn kilo_entry_shape_validation_matches_docs() {
        // Valid local + remote shapes.
        assert!(validate_entry_shape(
            &Editor::KiloCode,
            &json!({"type": "local", "command": ["contextstream-mcp"], "enabled": true})
        )
        .is_ok());
        assert!(validate_entry_shape(
            &Editor::KiloCode,
            &json!({"type": "remote", "url": "https://mcp.contextstream.io/mcp"})
        )
        .is_ok());

        // The historic breakage: generic "http" type is invalid for Kilo.
        let err = validate_entry_shape(&Editor::KiloCode, &json!({"type": "http", "url": "x"}))
            .unwrap_err();
        assert!(err.contains("invalid for Kilo"));

        assert!(validate_entry_shape(&Editor::KiloCode, &json!({"type": "local"})).is_err());
        assert!(validate_entry_shape(&Editor::KiloCode, &json!({"type": "remote"})).is_err());
        assert!(validate_entry_shape(&Editor::KiloCode, &json!({})).is_err());
    }

    #[test]
    fn generic_entry_shape_accepts_remote_and_command_forms() {
        assert!(validate_entry_shape(
            &Editor::Windsurf,
            &json!({"serverUrl": "https://mcp.contextstream.io/mcp"})
        )
        .is_ok());
        assert!(validate_entry_shape(
            &Editor::Cursor,
            &json!({"type": "http", "url": "https://mcp.contextstream.io/mcp"})
        )
        .is_ok());
        // Non-absolute commands can't be existence-checked — accepted.
        assert!(
            validate_entry_shape(&Editor::Cursor, &json!({"command": "contextstream-mcp"})).is_ok()
        );
        // Absolute-but-missing binary is a hard failure.
        assert!(validate_entry_shape(
            &Editor::Cursor,
            &json!({"command": "/definitely/not/here/contextstream-mcp"})
        )
        .is_err());
        assert!(validate_entry_shape(&Editor::Cursor, &json!({})).is_err());
    }

    #[test]
    fn mcp_config_file_check_reports_missing_parse_and_entry_states() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("kilo.jsonc");
        let installation_id = Uuid::from_u128(0x1234);
        const FIX: &str = "contextstream-mcp update-configs --scope global";

        // Missing file.
        let missing = check_mcp_config_file(
            &Editor::KiloCode,
            "mcp_global",
            &path,
            CheckStatus::Fail,
            Some(installation_id),
            FIX,
        );
        assert_eq!(missing.status, CheckStatus::Fail);
        assert!(missing.fix.is_some());

        // Unparseable file.
        std::fs::write(&path, "{ not json").unwrap();
        let unparseable = check_mcp_config_file(
            &Editor::KiloCode,
            "mcp_global",
            &path,
            CheckStatus::Fail,
            Some(installation_id),
            FIX,
        );
        assert_eq!(unparseable.status, CheckStatus::Fail);

        // Parseable but no contextstream entry.
        std::fs::write(
            &path,
            r#"{"mcp": {"other": {"type": "local", "command": ["x"]}}}"#,
        )
        .unwrap();
        let absent = check_mcp_config_file(
            &Editor::KiloCode,
            "mcp_global",
            &path,
            CheckStatus::Fail,
            Some(installation_id),
            FIX,
        );
        assert_eq!(absent.status, CheckStatus::Fail);
        assert!(absent.detail.contains("no contextstream"));

        // Healthy remote entry (with JSONC comment to prove json-like parsing).
        std::fs::write(
            &path,
            format!(
                concat!(
                    "// managed\n",
                    "{{\"mcp\": {{\"contextstream\": {{",
                    "\"type\": \"remote\",",
                    "\"url\": \"https://mcp.contextstream.io/mcp\",",
                    "\"headers\": {{",
                    "\"X-ContextStream-Client\": \"kilo\",",
                    "\"X-ContextStream-Managed-Config-Version\": \"{}\",",
                    "\"X-ContextStream-Teaching-Version\": \"{}\",",
                    "\"X-ContextStream-Installation-Id\": \"{}\"",
                    "}}}}}}}}"
                ),
                MANAGED_CONFIG_VERSION, HARNESS_TEACHING_VERSION, installation_id
            ),
        )
        .unwrap();
        let healthy = check_mcp_config_file(
            &Editor::KiloCode,
            "mcp_global",
            &path,
            CheckStatus::Fail,
            Some(installation_id),
            FIX,
        );
        assert_eq!(healthy.status, CheckStatus::Pass);

        let wrong_identity = check_mcp_config_file(
            &Editor::KiloCode,
            "mcp_global",
            &path,
            CheckStatus::Fail,
            Some(Uuid::from_u128(0x5678)),
            FIX,
        );
        assert_eq!(wrong_identity.status, CheckStatus::Fail);
        assert!(wrong_identity
            .detail
            .contains("does not match this installation"));
    }

    #[test]
    fn rules_file_check_requires_contextstream_block() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("contextstream.md");
        const FIX: &str = "contextstream-mcp update-rules --scope project";
        crate::setup::install_canonical_rules_hash();
        let canonical_hash =
            mcp_types::rules_hash::canonical_rules_hash().expect("canonical rules hash");

        let missing = check_rules_file("rules_project", &path, CheckStatus::Fail, FIX);
        assert_eq!(missing.status, CheckStatus::Fail);

        std::fs::write(&path, "# Unrelated rules\n").unwrap();
        let unrelated = check_rules_file("rules_project", &path, CheckStatus::Fail, FIX);
        assert_eq!(unrelated.status, CheckStatus::Fail);

        std::fs::write(
            &path,
            format!(
                "<contextstream>\n<!-- contextstream-rules-hash: {canonical_hash} -->\n# ContextStream Rules\n</contextstream>\n"
            ),
        )
        .unwrap();
        let managed = check_rules_file("rules_project", &path, CheckStatus::Fail, FIX);
        assert_eq!(managed.status, CheckStatus::Pass);

        let stale_hash = if canonical_hash == "0000000000000000" {
            "1111111111111111"
        } else {
            "0000000000000000"
        };
        std::fs::write(
            &path,
            format!(
                "<contextstream>\n<!-- contextstream-rules-hash: {stale_hash} -->\n# ContextStream Rules\n</contextstream>\n"
            ),
        )
        .unwrap();
        let stale = check_rules_file("rules_project", &path, CheckStatus::Fail, FIX);
        assert_eq!(stale.status, CheckStatus::Fail);
        assert!(stale.detail.contains("stale"));

        std::fs::write(
            &path,
            "<contextstream>\nUser-authored XML that mentions ContextStream.\n</contextstream>\n",
        )
        .unwrap();
        let unowned = check_rules_file("rules_project", &path, CheckStatus::Fail, FIX);
        assert_eq!(unowned.status, CheckStatus::Fail);
    }

    #[test]
    fn windsurf_entry_key_validation_flags_cursor_shape_leak() {
        assert!(windsurf_entry_keys_valid(&json!({
            "command": "\"/usr/local/bin/contextstream-mcp\" hook pre-tool-use",
            "show_output": true
        })));
        assert!(!windsurf_entry_keys_valid(&json!({
            "command": "x",
            "type": "command"
        })));
        assert!(!windsurf_entry_keys_valid(&json!({
            "command": "x",
            "timeout": 5
        })));
    }

    #[test]
    fn doctor_report_serializes_stable_schema() {
        let readiness = HarnessReadinessReport {
            harness_id: HarnessId::KiloCode,
            teaching_load_evidence: Editor::KiloCode.profile().teaching_load_evidence,
            local: unavailable_readiness_snapshot(&Editor::KiloCode, "not checked"),
            remote: unavailable_readiness_snapshot(&Editor::KiloCode, "not checked"),
        };
        let report = DoctorReport {
            schema_version: DOCTOR_REPORT_SCHEMA_VERSION,
            targeting: DoctorTargeting {
                scope: DoctorScope::All,
                source: "requested",
                editors: vec!["kilo"],
                detection_fallback_allowed: false,
            },
            editors: vec![EditorReport {
                editor: "kilo",
                editor_name: "Kilo Code",
                checks: vec![SurfaceCheck::pass("mcp_global", "ok", None)],
                readiness,
            }],
            installation: SurfaceCheck::pass("installation", "ok", None),
            credentials: SurfaceCheck::skipped("credentials", "not checked"),
            repair: None,
            pass: 2,
            warn: 0,
            fail: 0,
            skipped: 1,
        };
        let value = serde_json::to_value(&report).expect("serialize");
        assert_eq!(value["schema_version"], DOCTOR_REPORT_SCHEMA_VERSION);
        assert_eq!(value["targeting"]["source"], "requested");
        assert_eq!(value["targeting"]["editors"], json!(["kilo"]));
        assert_eq!(value["editors"][0]["editor"], "kilo");
        assert_eq!(value["editors"][0]["checks"][0]["surface"], "mcp_global");
        assert_eq!(value["editors"][0]["checks"][0]["status"], "pass");
        assert_eq!(value["installation"]["status"], "pass");
        assert_eq!(value["credentials"]["status"], "skipped");
        assert_eq!(value["pass"], 2);
        assert_eq!(
            value["editors"][0]["readiness"]["local"]["stages"]
                .as_array()
                .unwrap()
                .len(),
            HarnessReadinessStage::ALL.len()
        );
        assert!(!value.as_object().unwrap().contains_key("repair"));
        assert!(!value["editors"][0]["checks"][0]
            .as_object()
            .unwrap()
            .contains_key("fix"));
    }

    #[test]
    fn privacy_bounded_reports_exclude_local_diagnostics_and_evidence() {
        let private_path = Path::new("/home/alice/private-project/.codex/config.toml");
        let mut evidence = readiness_evidence(
            Editor::Codex,
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ContextTool,
        );
        evidence.teaching_version = Some("private-evidence-value".to_string());
        let readiness = HarnessReadinessReport {
            harness_id: HarnessId::Codex,
            teaching_load_evidence: Editor::Codex.profile().teaching_load_evidence,
            local: ReadinessSnapshot {
                state: ReadinessSnapshotState::Ready,
                highest_ready_stage: Some(HarnessReadinessStage::Grounded),
                stages: vec![ReadinessStageReport {
                    stage: HarnessReadinessStage::Grounded,
                    status: ReadinessEvidenceStatus::Verified,
                    evidence: vec![evidence],
                }],
                detail: Some("local detail with /home/alice and a token".to_string()),
            },
            remote: failed_readiness_snapshot(
                &Editor::Codex,
                "remote detail with private-server.example",
            ),
        };
        let report = DoctorReport {
            schema_version: DOCTOR_REPORT_SCHEMA_VERSION,
            targeting: DoctorTargeting {
                scope: DoctorScope::All,
                source: "requested",
                editors: vec!["codex"],
                detection_fallback_allowed: false,
            },
            editors: vec![EditorReport {
                editor: "codex",
                editor_name: "Codex",
                checks: vec![
                    SurfaceCheck::pass("mcp_global", "private detail", Some(private_path)),
                    SurfaceCheck::failure(
                        "hooks",
                        "secret diagnostic",
                        Some(private_path),
                        Some("private repair command"),
                    ),
                ],
                readiness,
            }],
            installation: SurfaceCheck::failure(
                "installation",
                "private installation detail",
                Some(private_path),
                None,
            ),
            credentials: SurfaceCheck::warning(
                "credentials",
                "private credential detail",
                Some(private_path),
                None,
            ),
            repair: Some(DoctorRepairReport {
                mode: "applied",
                operations: vec!["private-operation"],
                changes: vec![RepairChange {
                    path: private_path.display().to_string(),
                    action: "private-action",
                }],
                failures: vec!["private repair failure".to_string()],
            }),
            pass: 1,
            warn: 1,
            fail: 2,
            skipped: 0,
        };
        assert_eq!(
            support_command(&report),
            "contextstream-mcp doctor --support --scope all --editors codex"
        );

        let value = serde_json::to_value(report.completion_summary()).expect("serialize");
        assert_eq!(
            value["schema_version"],
            DOCTOR_COMPLETION_SUMMARY_SCHEMA_VERSION
        );
        assert_eq!(
            value["source_report_schema_version"],
            DOCTOR_REPORT_SCHEMA_VERSION
        );
        assert_eq!(value["editors"][0]["editor"], "codex");
        assert_eq!(value["editors"][0]["checks"]["pass"], 1);
        assert_eq!(value["editors"][0]["checks"]["fail"], 1);
        assert_eq!(value["editors"][0]["local_readiness"], "ready");
        assert_eq!(value["editors"][0]["remote_readiness"], "failed");
        assert_eq!(value["installation"], "fail");
        assert_eq!(value["credentials"], "warn");
        assert_eq!(value["has_failures"], true);

        let encoded = serde_json::to_string(&value).expect("encode");
        for private_fragment in [
            "/home/alice",
            "private-project",
            "private-server",
            "private-evidence",
            "private repair",
            "secret diagnostic",
            "\"path\"",
            "\"detail\"",
            "\"fix\"",
            "\"evidence\"",
            "\"repair\"",
        ] {
            assert!(
                !encoded.contains(private_fragment),
                "completion summary leaked {private_fragment}: {encoded}"
            );
        }

        let installation_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let support = doctor_support_report_from_parts(
            &report,
            DoctorTransportState::Hosted,
            Some(installation_id),
            DoctorSupportProject {
                state: DoctorSupportProjectState::ExactCheckout,
                workspace_id: Some(workspace_id),
                project_id: Some(project_id),
                checkout_id: Some("checkout-v1:11111111-1111-4111-8111-111111111111".to_string()),
                checkout_locator: Some("checkout-locator-v1:opaque".to_string()),
                repository_fingerprint: Some(
                    "git-common-dir-v1:22222222-2222-4222-8222-222222222222".to_string(),
                ),
            },
            DoctorSupportBridge {
                required: true,
                registration_readable: true,
                registration_state: Some(super::super::SyncBridgeRegistrationState::Registered),
                activation_state: Some(super::super::SyncBridgeActivationState::Active),
                platform: Some("linux_systemd_user"),
                health_state: Some(crate::watch::SyncBridgeHealthState::Running),
                heartbeat_fresh: Some(true),
                target_count: Some(2),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            },
        );
        let support_value = serde_json::to_value(&support).expect("serialize support report");
        assert_eq!(
            support_value["schema_version"],
            DOCTOR_SUPPORT_REPORT_SCHEMA_VERSION
        );
        assert_eq!(support_value["mcp_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(support_value["transport"], "hosted_remote");
        assert_eq!(
            support_value["installation_id"],
            installation_id.to_string()
        );
        assert_eq!(
            support_value["editors"][0]["activation_state"],
            "repair_required"
        );
        assert_eq!(
            support_value["editors"][0]["recommended_action"],
            "repair_managed_surfaces"
        );
        assert_eq!(
            support_value["editors"][0]["repair_command"],
            "contextstream-mcp doctor --repair --scope all --editors codex"
        );
        assert_eq!(support_value["project"]["state"], "exact_checkout");
        assert_eq!(
            support_value["project"]["workspace_id"],
            workspace_id.to_string()
        );
        assert_eq!(
            support_value["project"]["project_id"],
            project_id.to_string()
        );
        assert_eq!(support_value["sync_bridge"]["target_count"], 2);
        assert_eq!(support_value["summary"]["setup_healthy"], false);

        let support_encoded = serde_json::to_string(&support_value).expect("encode support report");
        for private_fragment in [
            "/home/alice",
            "private-project",
            "private-server",
            "private-evidence",
            "private repair",
            "private-operation",
            "private-action",
            "secret diagnostic",
            "\"path\"",
            "\"detail\"",
            "\"fix\"",
            "\"evidence\"",
            "\"git_branch\"",
            "\"git_commit",
            "\"repository_identity\"",
        ] {
            assert!(
                !support_encoded.contains(private_fragment),
                "support report leaked {private_fragment}: {support_encoded}"
            );
        }
    }

    fn readiness_evidence(
        editor: Editor,
        stage: HarnessReadinessStage,
        status: ReadinessEvidenceStatus,
        source: ReadinessEvidenceSource,
    ) -> HarnessReadinessEvidence {
        HarnessReadinessEvidence::new(
            editor.harness_id(),
            stage,
            status,
            source,
            chrono::Utc::now(),
        )
    }

    fn activation_report(
        editor: Editor,
        remote: ReadinessSnapshot,
        checks: Vec<SurfaceCheck>,
    ) -> DoctorReport {
        DoctorReport {
            schema_version: DOCTOR_REPORT_SCHEMA_VERSION,
            targeting: DoctorTargeting {
                scope: DoctorScope::All,
                source: "requested",
                editors: vec![editor.id()],
                detection_fallback_allowed: false,
            },
            editors: vec![EditorReport {
                editor: editor.id(),
                editor_name: editor.display_name(),
                checks,
                readiness: HarnessReadinessReport {
                    harness_id: editor.harness_id(),
                    teaching_load_evidence: editor.profile().teaching_load_evidence,
                    local: unavailable_readiness_snapshot(&editor, "not used"),
                    remote,
                },
            }],
            installation: SurfaceCheck::pass("installation", "ok", None),
            credentials: SurfaceCheck::pass("credentials", "ok", None),
            repair: None,
            pass: 2,
            warn: 0,
            fail: 0,
            skipped: 0,
        }
    }

    #[test]
    fn activation_requires_exact_verified_server_evidence() {
        let missing = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(&Editor::Codex, Vec::new()),
            Vec::new(),
        );
        assert_eq!(
            activation_guidance_for_editor(&missing.editors[0], false).state,
            ActivationGuidanceState::RestartRequired
        );

        let inferred_protocol = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Connected,
                    ReadinessEvidenceStatus::Inferred,
                    ReadinessEvidenceSource::McpProtocolRequest,
                )],
            ),
            Vec::new(),
        );
        assert_eq!(
            activation_guidance_for_editor(&inferred_protocol.editors[0], false).state,
            ActivationGuidanceState::RestartRequired
        );

        let wrong_source = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Connected,
                    ReadinessEvidenceStatus::Verified,
                    ReadinessEvidenceSource::RuntimeBehavior,
                )],
            ),
            Vec::new(),
        );
        assert_eq!(
            activation_guidance_for_editor(&wrong_source.editors[0], false).state,
            ActivationGuidanceState::RestartRequired
        );

        let stale = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Connected,
                    ReadinessEvidenceStatus::Stale,
                    ReadinessEvidenceSource::McpProtocolRequest,
                )],
            ),
            vec![SurfaceCheck::failure(
                "readiness_remote",
                "stale",
                None,
                Some("reload"),
            )],
        );
        assert_eq!(
            activation_guidance_for_editor(&stale.editors[0], false).state,
            ActivationGuidanceState::RestartRequired,
            "stale runtime evidence is a reload state, not broken setup"
        );
        let mut stale_report = stale;
        stale_report.fail = 1;
        assert!(stale_report.has_failures());
        assert!(
            !stale_report.has_setup_failures(),
            "runtime evidence alone must not turn healthy configuration into repair_required"
        );

        let connected = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Connected,
                    ReadinessEvidenceStatus::Verified,
                    ReadinessEvidenceSource::McpProtocolRequest,
                )],
            ),
            Vec::new(),
        );
        assert_eq!(
            activation_guidance_for_editor(&connected.editors[0], false).state,
            ActivationGuidanceState::ConnectionObserved
        );

        let grounded = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Grounded,
                    ReadinessEvidenceStatus::Verified,
                    ReadinessEvidenceSource::ContextTool,
                )],
            ),
            Vec::new(),
        );
        let guidance = activation_guidance_for_editor(&grounded.editors[0], false);
        assert_eq!(guidance.state, ActivationGuidanceState::GroundingObserved);
        assert!(guidance.status.contains("checkout identity"));
        assert!(guidance.status.contains("answer quality"));
        assert!(guidance.status.contains("not inferred"));
    }

    #[test]
    fn activation_is_truthful_for_rules_only_offline_and_failed_states() {
        let aider = activation_report(
            Editor::Aider,
            readiness_snapshot_from_evidence(&Editor::Aider, Vec::new()),
            Vec::new(),
        );
        let rules_only = activation_guidance_for_editor(&aider.editors[0], false);
        assert_eq!(rules_only.state, ActivationGuidanceState::RulesOnly);
        assert!(rules_only.status.contains("no MCP transport"));
        assert!(rules_only.next_step.contains("no MCP handshake"));

        let offline = activation_report(
            Editor::Codex,
            unavailable_readiness_snapshot(&Editor::Codex, "offline"),
            Vec::new(),
        );
        let unverified = activation_guidance_for_editor(&offline.editors[0], false);
        assert_eq!(
            unverified.state,
            ActivationGuidanceState::ConnectionUnverified
        );
        assert!(unverified.status.contains("cannot verify"));
        assert!(unverified.next_step.contains("hosted MCP"));

        let rejected = activation_report(
            Editor::Codex,
            failed_readiness_snapshot(&Editor::Codex, "rejected"),
            vec![SurfaceCheck::failure(
                "readiness_remote",
                "rejected",
                None,
                None,
            )],
        );
        assert_eq!(
            activation_guidance_for_editor(&rejected.editors[0], false).state,
            ActivationGuidanceState::RepairRequired
        );

        let failed = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(&Editor::Codex, Vec::new()),
            vec![SurfaceCheck::failure(
                "mcp_global",
                "broken",
                None,
                Some("repair"),
            )],
        );
        assert_eq!(
            activation_guidance_for_editor(&failed.editors[0], false).state,
            ActivationGuidanceState::RepairRequired
        );
        assert_eq!(
            activation_guidance_for_editor(&offline.editors[0], true).state,
            ActivationGuidanceState::RepairRequired
        );
    }

    #[test]
    fn activation_guidance_requires_an_exact_checkout_binding_before_first_value() {
        let connected = activation_report(
            Editor::Codex,
            readiness_snapshot_from_evidence(
                &Editor::Codex,
                vec![readiness_evidence(
                    Editor::Codex,
                    HarnessReadinessStage::Connected,
                    ReadinessEvidenceStatus::Verified,
                    ReadinessEvidenceSource::McpProtocolRequest,
                )],
            ),
            Vec::new(),
        );
        let unbound = tempfile::tempdir().expect("unbound checkout");
        let unbound_lines = activation_guidance_lines(&connected, Some(unbound.path())).join("\n");
        assert!(unbound_lines.contains("No exact checkout binding was confirmed"));
        assert!(unbound_lines.contains("setup --project-path . --editors codex"));
        assert!(!unbound_lines.contains(super::super::first_value_prompt()));

        let bound = tempfile::tempdir().expect("bound checkout");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let config_dir = bound.path().join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("config directory");
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&json!({
                "workspace_id": workspace_id,
                "project_id": project_id,
                "checkout_root": super::super::canonical_checkout_root(bound.path()),
                "configured_editors": ["codex"]
            }))
            .expect("serialize binding"),
        )
        .expect("write binding");

        let bound_lines = activation_guidance_lines(&connected, Some(bound.path())).join("\n");
        assert!(bound_lines.contains("project(action=\"index_status\")"));
        assert!(bound_lines.contains("checkout_index_unconfirmed"));
        assert!(bound_lines.contains("keep hosted MCP configured"));
        assert!(bound_lines.contains(super::super::first_value_prompt()));
    }

    #[test]
    fn rules_only_activation_does_not_offer_mcp_or_index_steps() {
        let aider = activation_report(
            Editor::Aider,
            readiness_snapshot_from_evidence(&Editor::Aider, Vec::new()),
            Vec::new(),
        );
        let lines = activation_guidance_lines(&aider, None).join("\n");
        assert!(lines.contains("rules-only"));
        assert!(!lines.contains("index_status"));
        assert!(!lines.contains("First-value prompt:"));

        let broken_aider = activation_report(
            Editor::Aider,
            readiness_snapshot_from_evidence(&Editor::Aider, Vec::new()),
            vec![SurfaceCheck::failure(
                "rules_global",
                "missing",
                None,
                Some("repair Aider rules"),
            )],
        );
        let broken_lines = activation_guidance_lines(&broken_aider, None).join("\n");
        assert!(broken_lines.contains("repair Aider rules"));
        assert!(!broken_lines.contains("index_status"));
        assert!(!broken_lines.contains(super::super::first_value_prompt()));
    }

    #[test]
    fn conflicting_readiness_evidence_fails_closed() {
        let verified = readiness_evidence(
            Editor::Codex,
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ContextTool,
        );
        let failed = readiness_evidence(
            Editor::Codex,
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Failed,
            ReadinessEvidenceSource::ComplianceCheck,
        );

        let snapshot = readiness_snapshot_from_evidence(&Editor::Codex, vec![verified, failed]);
        assert_eq!(snapshot.state, ReadinessSnapshotState::Failed);
        assert_eq!(
            snapshot
                .stages
                .iter()
                .find(|stage| stage.stage == HarnessReadinessStage::Grounded)
                .unwrap()
                .status,
            ReadinessEvidenceStatus::Failed
        );
    }

    #[test]
    fn stale_versioned_evidence_is_not_reported_ready() {
        let mut configured = readiness_evidence(
            Editor::Codex,
            HarnessReadinessStage::Configured,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ManagedMcpConfig,
        );
        configured.managed_config_version = Some("stale-version".to_string());
        let effective = effective_evidence_records_for(
            &[configured],
            HarnessId::Codex,
            current_versions(&Editor::Codex),
        );

        let snapshot = readiness_snapshot_from_evidence(&Editor::Codex, effective);
        assert_eq!(snapshot.state, ReadinessSnapshotState::Stale);
        assert_eq!(snapshot.highest_ready_stage, None);
    }

    #[test]
    fn readiness_distinguishes_missing_not_observable_and_ready() {
        let missing = readiness_snapshot_from_evidence(&Editor::Codex, Vec::new());
        assert_eq!(missing.state, ReadinessSnapshotState::Missing);

        let mut taught = readiness_evidence(
            Editor::Aider,
            HarnessReadinessStage::Taught,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ManagedRules,
        );
        taught.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
        let not_observable = readiness_snapshot_from_evidence(&Editor::Aider, vec![taught]);
        assert_eq!(not_observable.state, ReadinessSnapshotState::NotObservable);

        let grounded = readiness_evidence(
            Editor::Codex,
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ContextTool,
        );
        let ready = readiness_snapshot_from_evidence(&Editor::Codex, vec![grounded]);
        assert_eq!(ready.state, ReadinessSnapshotState::Ready);
        assert_eq!(
            ready.highest_ready_stage,
            Some(HarnessReadinessStage::Grounded)
        );
    }

    #[test]
    fn local_readiness_rejects_an_orphaned_installation_ledger() {
        let ledger_installation_id = Uuid::from_u128(0x1111);
        let current_installation_id = Uuid::from_u128(0x2222);
        let now = chrono::Utc::now();
        let ledger = HarnessReadinessLedger {
            schema_version: mcp_client::harness_readiness::HARNESS_READINESS_LEDGER_SCHEMA_VERSION,
            installation_id: ledger_installation_id,
            created_at: now,
            updated_at: now,
            evidence: vec![readiness_evidence(
                Editor::Codex,
                HarnessReadinessStage::Grounded,
                ReadinessEvidenceStatus::Verified,
                ReadinessEvidenceSource::ContextTool,
            )],
        };

        let snapshot = local_readiness_snapshot(
            &Editor::Codex,
            Some(current_installation_id),
            Ok(Some(&ledger)),
        );
        assert_eq!(snapshot.state, ReadinessSnapshotState::Failed);
        assert!(snapshot
            .detail
            .as_deref()
            .unwrap()
            .contains("different or missing installation"));
    }

    #[test]
    fn remote_failures_are_privacy_bounded_and_offline_is_nonfatal() {
        let offline = remote_readiness_snapshot(
            &Editor::Codex,
            Err(McpError::Network(
                "secret token and /home/private/path".to_string(),
            )),
        );
        assert_eq!(offline.state, ReadinessSnapshotState::Unavailable);
        assert!(!offline.detail.as_deref().unwrap().contains("secret"));
        assert!(!offline.detail.as_deref().unwrap().contains("/home"));

        let rejected = remote_readiness_snapshot(
            &Editor::Codex,
            Err(McpError::Validation(
                "server echoed another installation and secret".to_string(),
            )),
        );
        assert_eq!(rejected.state, ReadinessSnapshotState::Failed);
        assert!(!rejected.detail.as_deref().unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn offline_credential_probe_warns_without_echoing_connection_details() {
        let client = ContextStreamClient::new(Config {
            api_url: "http://127.0.0.1:0/api/v1".to_string(),
            api_key: Some("doctor-test-secret".to_string()),
            ..Default::default()
        });
        let probe = probe_credentials_client(client, std::time::Duration::from_millis(250)).await;

        assert_eq!(probe.check.status, CheckStatus::Warn);
        assert!(probe.client.is_none());
        assert!(!probe.check.detail.contains("doctor-test-secret"));
        assert!(!probe.check.detail.contains("127.0.0.1"));
    }

    #[test]
    fn explicit_repair_targets_bypass_historical_selection_state() {
        let mut options = DoctorOptions::new(None);
        options.explicit_editors = Some(vec![Editor::Codex, Editor::Codex]);
        options.repair = true;

        let (targets, source, detection_allowed) =
            resolve_doctor_targets(&options).expect("explicit target resolution");
        assert_eq!(targets, vec![Editor::Codex]);
        assert_eq!(source, "requested");
        assert!(!detection_allowed);
    }

    #[test]
    fn repair_requires_explicit_scope_and_matching_project_directory() {
        let mut global = DoctorOptions::new(None);
        global.repair = true;
        global.scope = DoctorScope::Global;
        assert!(validate_repair_scope(&global)
            .expect_err("implicit repair scope must be refused")
            .to_string()
            .contains("explicit --scope"));

        global.scope_was_explicit = true;
        validate_repair_scope(&global).expect("explicit global repair is safe");

        let other_directory = tempfile::tempdir().expect("other directory");
        let mut project = DoctorOptions::new(Some(other_directory.path().to_path_buf()));
        project.repair = true;
        project.scope = DoctorScope::Project;
        project.scope_was_explicit = true;
        assert!(validate_repair_scope(&project)
            .expect_err("repair must not inspect one directory and mutate another")
            .to_string()
            .contains("not the current working directory"));
    }

    #[test]
    fn doctor_dry_run_guard_clears_process_state_and_plans() {
        super::super::safe_edit::set_dry_run(true);
        super::super::safe_edit::record_external_change(
            Path::new("/doctor-dry-run-test"),
            super::super::safe_edit::ChangeAction::Modify,
        );
        {
            let _reset = DoctorDryRunReset;
        }
        assert!(!super::super::safe_edit::is_dry_run());
        assert!(super::super::safe_edit::take_planned_changes().is_empty());
    }

    #[test]
    fn codex_check_requires_a_real_owned_table_and_current_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        let installation_id = Uuid::from_u128(0x1234);
        const FIX: &str = "contextstream-mcp update-configs --scope global --editors codex";

        let mention_only = check_codex_mcp_config(
            "mcp_global",
            &path,
            "# my-contextstream-sync.sh\nmodel = \"gpt\"\n",
            Some(installation_id),
            FIX,
        );
        assert_eq!(mention_only.status, CheckStatus::Fail);

        let managed = format!(
            concat!(
                "[mcp_servers.contextstream]\n",
                "url = \"https://mcp.contextstream.io/mcp\"\n",
                "[mcp_servers.contextstream.http_headers]\n",
                "\"X-ContextStream-Client\" = \"codex\"\n",
                "\"X-ContextStream-Managed-Config-Version\" = \"{}\"\n",
                "\"X-ContextStream-Teaching-Version\" = \"{}\"\n",
                "\"X-ContextStream-Installation-Id\" = \"{}\"\n"
            ),
            MANAGED_CONFIG_VERSION, HARNESS_TEACHING_VERSION, installation_id
        );
        let healthy =
            check_codex_mcp_config("mcp_global", &path, &managed, Some(installation_id), FIX);
        assert_eq!(healthy.status, CheckStatus::Pass);
    }
}
