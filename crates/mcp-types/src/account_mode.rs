//! Team vs personal execution mode for MCP tools.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use uuid::Uuid;

/// Startup / persisted preference. `Auto` follows account defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountModePreference {
    #[default]
    Auto,
    Team,
    Personal,
}

impl AccountModePreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Team => "team",
            Self::Personal => "personal",
        }
    }
}

impl FromStr for AccountModePreference {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "team" | "shared" | "workspace" => Ok(Self::Team),
            "personal" | "private" | "individual" => Ok(Self::Personal),
            _ => Err(()),
        }
    }
}

/// Resolved execution mode that controls default read/write scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Team,
    Personal,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Personal => "personal",
        }
    }

    pub fn default_is_personal(self) -> bool {
        matches!(self, Self::Personal)
    }
}

/// Snapshot of account context from the API (or MCP fallback).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountContextSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// `individual`, `team`, or `dual`.
    pub account_type: String,
    pub has_team_membership: bool,
    #[serde(default)]
    pub team_capabilities: Vec<String>,
    /// Selected billing/visibility context: `personal` or `team`.
    pub selected_context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_plan: Option<String>,
    /// Always `owner_only` for transcript content; topic signals may be team-visible.
    #[serde(default = "default_transcript_sharing")]
    pub transcript_sharing: String,
    #[serde(default)]
    pub source: AccountContextSource,
}

fn default_transcript_sharing() -> String {
    "owner_only".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountContextSource {
    #[default]
    Unknown,
    Api,
    AuthMe,
    Heuristic,
}

impl AccountContextSnapshot {
    pub fn team_features_available(&self) -> bool {
        self.has_team_membership
            || self.account_type.eq_ignore_ascii_case("team")
            || self.account_type.eq_ignore_ascii_case("dual")
    }

    pub fn is_dual_context(&self) -> bool {
        self.account_type.eq_ignore_ascii_case("dual")
    }
}

/// One urgent team item for context surfacing.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TeamPriorityItem {
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
}

/// Metadata-only transcript topic signal (no transcript body).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TranscriptTopicSignal {
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_discussed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participant_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
    #[serde(default)]
    pub content_shared: bool,
}

/// Team discussion record (separate from private transcripts).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TeamDiscussion {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}
