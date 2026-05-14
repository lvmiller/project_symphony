use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<BlockerRef>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    pub fn required_fields_present(&self) -> bool {
        !(self.id.is_empty()
            || self.identifier.is_empty()
            || self.title.is_empty()
            || self.state.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub config: serde_yaml::Mapping,
    pub prompt_template: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    PreparingWorkspace,
    BuildingPrompt,
    LaunchingAgentProcess,
    InitializingSession,
    StreamingTurn,
    Finishing,
    Succeeded,
    Failed,
    TimedOut,
    Stalled,
    CanceledByReconciliation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunAttempt {
    pub issue_id: String,
    pub issue_identifier: String,
    pub attempt: Option<u32>,
    pub workspace_path: PathBuf,
    pub started_at: DateTime<Utc>,
    pub status: RunStatus,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSession {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub codex_app_server_pid: Option<u32>,
    pub last_codex_event: Option<String>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_message: Option<String>,
    pub codex_tokens: TokenTotals,
    pub last_reported_tokens: TokenTotals,
    pub turn_count: u32,
}

impl LiveSession {
    pub fn new(thread_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        let thread_id = thread_id.into();
        let turn_id = turn_id.into();
        Self {
            session_id: format!("{thread_id}-{turn_id}"),
            thread_id,
            turn_id,
            codex_app_server_pid: None,
            last_codex_event: None,
            last_codex_timestamp: None,
            last_codex_message: None,
            codex_tokens: TokenTotals::default(),
            last_reported_tokens: TokenTotals::default(),
            turn_count: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at_ms: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub running: Vec<RunningSnapshot>,
    pub retrying: Vec<RetryEntry>,
    pub codex_totals: TokenTotals,
    pub seconds_running: f64,
    pub rate_limits: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunningSnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub state: String,
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexEvent {
    pub issue_id: String,
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
    pub message: Option<String>,
    pub absolute_token_totals: Option<TokenTotals>,
    pub rate_limits: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerExitReason {
    Normal,
    Failed(String),
    TimedOut(String),
    Stalled(String),
    CanceledByReconciliation,
}

impl WorkerExitReason {
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }

    pub fn error_message(&self) -> Option<String> {
        match self {
            Self::Normal => None,
            Self::Failed(message) | Self::TimedOut(message) | Self::Stalled(message) => {
                Some(message.clone())
            }
            Self::CanceledByReconciliation => Some("canceled by reconciliation".to_string()),
        }
    }
}

pub type StateCounts = BTreeMap<String, usize>;
