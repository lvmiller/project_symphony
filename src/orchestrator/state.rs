use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::{DEFAULT_SOURCE_ID, EffectiveConfig, normalize_state};
use crate::domain::{
    CodexEvent, ExecutionTarget, Issue, IssueSnapshot, LiveSession, RecentEvent, RetryEntry,
    RetrySnapshot, RunningSnapshot, RuntimeSnapshot, RuntimeSnapshotCounts, StateCounts,
    TokenTotals, WorkerExitReason,
};
use crate::orchestrator::retry::{
    continuation_retry_due_at_ms, failure_retry_delay_ms, failure_retry_due_at_ms,
};
use crate::time::{ms_from_now, system_monotonic_ms, utc_elapsed_ms};
use crate::workspace::source_workspace_key as workspace_source_workspace_key;

pub const RECENT_EVENT_HISTORY_LIMIT: usize = 32;
pub const RECENT_EVENT_MESSAGE_LIMIT_BYTES: usize = 1_024;

#[derive(Clone, Debug)]
pub struct RunningEntry {
    pub issue: Issue,
    pub source_id: String,
    pub identifier: String,
    pub workspace_key: String,
    pub started_at: DateTime<Utc>,
    pub retry_attempt: Option<u32>,
    pub execution_target: ExecutionTarget,
    pub workspace_path: std::path::PathBuf,
    pub live_session: Option<LiveSession>,
    pub cancel_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileDecision {
    NoRunningEntry,
    RefreshedActive,
    CancelTerminal,
    CancelNonActive,
    MissingFromTracker,
}

#[derive(Clone, Debug, Default)]
pub struct OrchestratorState {
    pub running: BTreeMap<String, RunningEntry>,
    pub claimed: BTreeSet<String>,
    pub claimed_issue_ids: BTreeSet<String>,
    pub claimed_workspace_keys: BTreeSet<String>,
    pub retry_attempts: BTreeMap<String, RetryEntry>,
    pub completed: BTreeSet<String>,
    pub codex_totals: TokenTotals,
    pub ended_runtime_seconds: f64,
    pub codex_rate_limits: Option<Value>,
    recent_events: BTreeMap<String, VecDeque<RecentEvent>>,
}

impl OrchestratorState {
    pub fn claim_running(&mut self, issue: Issue, attempt: Option<u32>, started_at: DateTime<Utc>) {
        self.claim_running_for_source(DEFAULT_SOURCE_ID, issue, attempt, started_at);
    }

    pub fn claim_running_for_source(
        &mut self,
        source_id: &str,
        issue: Issue,
        attempt: Option<u32>,
        started_at: DateTime<Utc>,
    ) {
        self.claim_running_on_target_for_source(
            source_id,
            issue,
            attempt,
            ExecutionTarget::Local,
            std::path::PathBuf::new(),
            started_at,
        );
    }

    pub fn claim_running_on_target_for_source(
        &mut self,
        source_id: &str,
        issue: Issue,
        attempt: Option<u32>,
        execution_target: ExecutionTarget,
        workspace_path: std::path::PathBuf,
        started_at: DateTime<Utc>,
    ) {
        let issue_key = source_issue_key(source_id, &issue.id);
        let issue_id = issue.id.clone();
        let identifier = issue.identifier.clone();
        let workspace_key = source_workspace_key(source_id, &identifier);
        self.claimed.insert(issue_key.clone());
        self.claimed_issue_ids.insert(issue_id.clone());
        self.claimed_workspace_keys.insert(workspace_key.clone());
        let displaced_retry_workspace_key = self
            .retry_attempts
            .remove(&issue_key)
            .map(|retry| retry.workspace_key);
        let displaced_running_workspace_key = self
            .running
            .get(&issue_key)
            .map(|entry| entry.workspace_key.clone());
        self.running.insert(
            issue_key,
            RunningEntry {
                source_id: source_id.to_string(),
                issue,
                identifier,
                workspace_key,
                execution_target,
                workspace_path,
                started_at,
                retry_attempt: attempt,
                live_session: None,
                cancel_requested: false,
            },
        );
        for workspace_key in displaced_retry_workspace_key
            .into_iter()
            .chain(displaced_running_workspace_key)
        {
            self.release_workspace_key_if_unowned(&workspace_key);
        }
        self.release_tracker_issue_id_if_unowned(&issue_id);
    }

    pub fn running_state_counts(&self) -> StateCounts {
        let mut counts = BTreeMap::new();
        for entry in self.running.values() {
            *counts
                .entry(normalize_state(&entry.issue.state))
                .or_insert(0) += 1;
        }
        counts
    }

    pub fn is_issue_or_workspace_claimed(&self, issue: &Issue) -> bool {
        self.is_issue_or_workspace_claimed_for_source(DEFAULT_SOURCE_ID, issue)
    }

    pub fn is_issue_or_workspace_claimed_for_source(&self, source_id: &str, issue: &Issue) -> bool {
        self.claimed
            .contains(&source_issue_key(source_id, &issue.id))
            || self.claimed_issue_ids.contains(&issue.id)
            || self
                .claimed_workspace_keys
                .contains(&source_workspace_key(source_id, &issue.identifier))
    }

    pub fn running_issue_ids_for_source(&self, source_id: &str) -> Vec<String> {
        self.running
            .values()
            .filter(|entry| entry.source_id == source_id)
            .map(|entry| entry.issue.id.clone())
            .collect()
    }

    pub fn running_entry_mut_for_source(
        &mut self,
        source_id: &str,
        issue_id: &str,
    ) -> Option<&mut RunningEntry> {
        let issue_key = source_issue_key(source_id, issue_id);
        self.running.get_mut(&issue_key)
    }

    pub fn due_retry_keys_for_source(&self, source_id: &str, now_ms: u64) -> Vec<String> {
        self.retry_attempts
            .iter()
            .filter(|(_, retry)| retry.source_id == source_id && retry.due_at_ms <= now_ms)
            .map(|(issue_key, _)| issue_key.clone())
            .collect()
    }

    pub fn next_retry_due_at_ms(&self) -> Option<u64> {
        self.retry_attempts
            .values()
            .map(|retry| retry.due_at_ms)
            .min()
    }

    pub fn apply_codex_event(&mut self, event: CodexEvent) {
        let summary = RecentEvent {
            at: event.timestamp,
            event: event.event.clone(),
            message: event.message.as_deref().map(bounded_recent_event_message),
        };
        let applied_to_running_issue = if let Some(entry) = self.running.get_mut(&event.issue_id) {
            if let (Some(thread_id), Some(turn_id)) =
                (event.thread_id.clone(), event.turn_id.clone())
            {
                let session = entry
                    .live_session
                    .get_or_insert_with(|| LiveSession::new(thread_id.clone(), turn_id.clone()));
                if session.thread_id != thread_id || session.turn_id != turn_id {
                    *session = LiveSession::new(thread_id, turn_id);
                }
                session.session_id = event
                    .session_id
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", session.thread_id, session.turn_id));
                session.codex_app_server_pid =
                    event.codex_app_server_pid.or(session.codex_app_server_pid);
                session.last_codex_event = Some(event.event.clone());
                session.last_codex_timestamp = Some(event.timestamp);
                session.last_codex_message = event.message.clone();
                if event.event == "turn_started" {
                    session.turn_count = session.turn_count.saturating_add(1);
                }
                if let Some(total) = event.absolute_token_totals.clone() {
                    let input_delta =
                        total.input_tokens - session.last_reported_tokens.input_tokens;
                    let output_delta =
                        total.output_tokens - session.last_reported_tokens.output_tokens;
                    let total_delta =
                        total.total_tokens - session.last_reported_tokens.total_tokens;
                    if input_delta > 0 {
                        self.codex_totals.input_tokens += input_delta;
                    }
                    if output_delta > 0 {
                        self.codex_totals.output_tokens += output_delta;
                    }
                    if total_delta > 0 {
                        self.codex_totals.total_tokens += total_delta;
                    }
                    session.last_reported_tokens = total.clone();
                    session.codex_tokens = total;
                }
            } else if let Some(session) = &mut entry.live_session {
                session.last_codex_event = Some(event.event.clone());
                session.last_codex_timestamp = Some(event.timestamp);
                session.last_codex_message = event.message.clone();
            }
            true
        } else {
            false
        };
        if applied_to_running_issue {
            let events = self
                .recent_events
                .entry(event.issue_id.clone())
                .or_default();
            events.push_back(summary);
            if events.len() > RECENT_EVENT_HISTORY_LIMIT {
                let _ = events.pop_front();
            }
        }
        if let Some(rate_limits) = event.rate_limits {
            self.codex_rate_limits = Some(rate_limits);
        }
    }

    pub fn worker_exit(
        &mut self,
        issue_id: &str,
        reason: WorkerExitReason,
        config: &EffectiveConfig,
        now_ms: u64,
        now_utc: DateTime<Utc>,
    ) -> Option<RetryEntry> {
        self.worker_exit_for_source(DEFAULT_SOURCE_ID, issue_id, reason, config, now_ms, now_utc)
    }

    pub fn worker_exit_for_source(
        &mut self,
        source_id: &str,
        issue_id: &str,
        reason: WorkerExitReason,
        config: &EffectiveConfig,
        now_ms: u64,
        now_utc: DateTime<Utc>,
    ) -> Option<RetryEntry> {
        let issue_key = source_issue_key(source_id, issue_id);
        self.worker_exit_by_key(&issue_key, reason, config, now_ms, now_utc)
    }

    pub fn worker_exit_by_key(
        &mut self,
        issue_key: &str,
        reason: WorkerExitReason,
        config: &EffectiveConfig,
        now_ms: u64,
        now_utc: DateTime<Utc>,
    ) -> Option<RetryEntry> {
        let entry = self.running.remove(issue_key)?;
        let issue_id = entry.issue.id.clone();
        self.ended_runtime_seconds += now_utc
            .signed_duration_since(entry.started_at)
            .num_milliseconds()
            .max(0) as f64
            / 1000.0;
        let (attempt, due_at_ms, error) = if reason.is_normal() {
            self.completed.insert(issue_key.to_string());
            (1, continuation_retry_due_at_ms(now_ms), None)
        } else {
            let next_attempt = entry.retry_attempt.unwrap_or(0).saturating_add(1).max(1);
            (
                next_attempt,
                failure_retry_due_at_ms(next_attempt, config.agent.max_retry_backoff_ms, now_ms),
                reason.error_message(),
            )
        };
        let retry = RetryEntry {
            source_id: entry.source_id,
            issue_id,
            identifier: entry.identifier,
            execution_target: entry.execution_target,
            workspace_path: entry.workspace_path,
            workspace_key: entry.workspace_key,
            attempt,
            due_at_ms,
            error,
        };
        self.requeue_retry(retry.clone());
        Some(retry)
    }

    pub fn release(&mut self, issue_id: &str) {
        self.release_for_source(DEFAULT_SOURCE_ID, issue_id);
    }

    pub fn release_for_source(&mut self, source_id: &str, issue_id: &str) {
        let issue_key = source_issue_key(source_id, issue_id);
        let mut workspace_keys = Vec::new();
        if let Some(entry) = self.running.remove(&issue_key) {
            workspace_keys.push(entry.workspace_key);
        }
        if let Some(retry) = self.retry_attempts.remove(&issue_key) {
            workspace_keys.push(retry.workspace_key);
        }
        self.recent_events.remove(&issue_key);
        self.claimed.remove(&issue_key);
        self.release_tracker_issue_id_if_unowned(issue_id);
        for workspace_key in workspace_keys {
            self.release_workspace_key_if_unowned(&workspace_key);
        }
    }

    pub fn reconcile_running_issue(
        &mut self,
        issue_id: &str,
        latest: Option<&Issue>,
        config: &EffectiveConfig,
    ) -> ReconcileDecision {
        self.reconcile_running_issue_for_source(DEFAULT_SOURCE_ID, issue_id, latest, config)
    }

    pub fn reconcile_running_issue_for_source(
        &mut self,
        source_id: &str,
        issue_id: &str,
        latest: Option<&Issue>,
        config: &EffectiveConfig,
    ) -> ReconcileDecision {
        let issue_key = source_issue_key(source_id, issue_id);
        let Some(entry) = self.running.get_mut(&issue_key) else {
            return ReconcileDecision::NoRunningEntry;
        };
        let Some(latest) = latest else {
            entry.cancel_requested = true;
            return ReconcileDecision::MissingFromTracker;
        };
        if config.is_terminal_state(&latest.state) {
            entry.issue = latest.clone();
            entry.identifier = latest.identifier.clone();
            entry.cancel_requested = true;
            return ReconcileDecision::CancelTerminal;
        }
        if !config.is_active_state(&latest.state) {
            entry.issue = latest.clone();
            entry.identifier = latest.identifier.clone();
            entry.cancel_requested = true;
            return ReconcileDecision::CancelNonActive;
        }
        entry.issue = latest.clone();
        entry.identifier = latest.identifier.clone();
        ReconcileDecision::RefreshedActive
    }

    pub fn stalled_issue_ids(&self, config: &EffectiveConfig, now: DateTime<Utc>) -> Vec<String> {
        self.stalled_issue_ids_for_source(DEFAULT_SOURCE_ID, config, now)
    }

    pub fn stalled_issue_ids_for_source(
        &self,
        source_id: &str,
        config: &EffectiveConfig,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        if config.codex.stall_timeout_ms <= 0 {
            return Vec::new();
        }
        let timeout_ms = config.codex.stall_timeout_ms as u64;
        self.running
            .values()
            .filter(|entry| entry.source_id == source_id)
            .filter_map(|entry| {
                let since = entry
                    .live_session
                    .as_ref()
                    .and_then(|session| session.last_codex_timestamp)
                    .unwrap_or(entry.started_at);
                (utc_elapsed_ms(since, now) > timeout_ms).then(|| entry.issue.id.clone())
            })
            .collect()
    }

    pub fn snapshot(&self, now: DateTime<Utc>) -> RuntimeSnapshot {
        self.snapshot_at(now, system_monotonic_ms())
    }

    pub fn snapshot_at(&self, now: DateTime<Utc>, observed_monotonic_ms: u64) -> RuntimeSnapshot {
        let running = self
            .running
            .iter()
            .map(|(issue_key, entry)| self.running_snapshot(issue_key, entry))
            .collect::<Vec<_>>();
        let retrying = self
            .retry_attempts
            .iter()
            .map(|(issue_key, retry)| self.retry_snapshot(issue_key, retry, observed_monotonic_ms))
            .collect::<Vec<_>>();
        let active_seconds = self
            .running
            .values()
            .map(|entry| {
                now.signed_duration_since(entry.started_at)
                    .num_milliseconds()
                    .max(0) as f64
                    / 1000.0
            })
            .sum::<f64>();
        RuntimeSnapshot {
            counts: RuntimeSnapshotCounts {
                running: running.len(),
                retrying: retrying.len(),
            },
            running,
            retrying,
            codex_totals: self.codex_totals.clone(),
            seconds_running: self.ended_runtime_seconds + active_seconds,
            rate_limits: self.codex_rate_limits.clone(),
        }
    }

    pub fn issue_snapshot(
        &self,
        issue_identifier: &str,
        observed_monotonic_ms: u64,
    ) -> IssueSnapshot {
        if let Some((issue_key, entry)) = self
            .running
            .iter()
            .find(|(_, entry)| entry.identifier == issue_identifier)
        {
            return IssueSnapshot::Running(self.running_snapshot(issue_key, entry));
        }
        self.retry_attempts
            .iter()
            .find(|(_, retry)| retry.identifier == issue_identifier)
            .map(|(issue_key, retry)| {
                IssueSnapshot::Retrying(self.retry_snapshot(
                    issue_key,
                    retry,
                    observed_monotonic_ms,
                ))
            })
            .unwrap_or(IssueSnapshot::NotFound)
    }

    fn running_snapshot(&self, issue_key: &str, entry: &RunningEntry) -> RunningSnapshot {
        let session = entry.live_session.as_ref();
        RunningSnapshot {
            source_id: entry.source_id.clone(),
            execution_target: entry.execution_target.clone(),
            workspace_path: entry.workspace_path.clone(),
            issue_id: entry.issue.id.clone(),
            issue_identifier: entry.identifier.clone(),
            state: entry.issue.state.clone(),
            workspace_key: entry.workspace_key.clone(),
            retry_attempt: entry.retry_attempt,
            cancel_requested: entry.cancel_requested,
            session_id: session.map(|session| session.session_id.clone()),
            thread_id: session.map(|session| session.thread_id.clone()),
            turn_id: session.map(|session| session.turn_id.clone()),
            codex_app_server_pid: session.and_then(|session| session.codex_app_server_pid),
            turn_count: session.map(|session| session.turn_count).unwrap_or(0),
            last_event: session.and_then(|session| session.last_codex_event.clone()),
            last_message: session.and_then(|session| session.last_codex_message.clone()),
            last_event_at: session.and_then(|session| session.last_codex_timestamp),
            tokens: session
                .map(|session| session.codex_tokens.clone())
                .unwrap_or_default(),
            started_at: entry.started_at,
            recent_events: self.recent_events_for_key(issue_key),
        }
    }

    fn retry_snapshot(
        &self,
        issue_key: &str,
        retry: &RetryEntry,
        observed_monotonic_ms: u64,
    ) -> RetrySnapshot {
        RetrySnapshot {
            source_id: retry.source_id.clone(),
            execution_target: retry.execution_target.clone(),
            workspace_path: retry.workspace_path.clone(),
            issue_id: retry.issue_id.clone(),
            issue_identifier: retry.identifier.clone(),
            workspace_key: retry.workspace_key.clone(),
            attempt: retry.attempt,
            error: retry.error.clone(),
            remaining_delay_ms: retry.due_at_ms.saturating_sub(observed_monotonic_ms),
            recent_events: self.recent_events_for_key(issue_key),
        }
    }

    fn recent_events_for_key(&self, issue_key: &str) -> Vec<RecentEvent> {
        self.recent_events
            .get(issue_key)
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn schedule_retry_now(
        &mut self,
        issue: &Issue,
        attempt: u32,
        error: impl Into<Option<String>>,
    ) -> RetryEntry {
        self.schedule_retry_now_for_source(DEFAULT_SOURCE_ID, issue, attempt, error)
    }

    pub fn schedule_retry_now_for_source(
        &mut self,
        source_id: &str,
        issue: &Issue,
        attempt: u32,
        error: impl Into<Option<String>>,
    ) -> RetryEntry {
        let retry = RetryEntry {
            source_id: source_id.to_string(),
            execution_target: ExecutionTarget::Local,
            workspace_path: std::path::PathBuf::new(),
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            workspace_key: source_workspace_key(source_id, &issue.identifier),
            attempt,
            due_at_ms: ms_from_now(failure_retry_delay_ms(attempt, u64::MAX)),
            error: error.into(),
        };
        let issue_key = source_issue_key(source_id, &issue.id);
        let displaced_retry_workspace_key = self
            .retry_attempts
            .insert(issue_key, retry.clone())
            .map(|retry| retry.workspace_key);
        self.claim_retry(&retry);
        if let Some(workspace_key) = displaced_retry_workspace_key {
            self.release_workspace_key_if_unowned(&workspace_key);
        }
        retry
    }

    pub fn requeue_retry(&mut self, retry: RetryEntry) {
        let issue_key = source_issue_key(&retry.source_id, &retry.issue_id);
        self.retry_attempts.insert(issue_key, retry.clone());
        self.claim_retry(&retry);
    }

    pub fn release_retry_claim(&mut self, retry: &RetryEntry) {
        let issue_key = source_issue_key(&retry.source_id, &retry.issue_id);
        self.claimed.remove(&issue_key);
        self.release_tracker_issue_id_if_unowned(&retry.issue_id);
        self.release_workspace_key_if_unowned(&retry.workspace_key);
    }

    fn claim_retry(&mut self, retry: &RetryEntry) {
        self.claimed
            .insert(source_issue_key(&retry.source_id, &retry.issue_id));
        self.claimed_issue_ids.insert(retry.issue_id.clone());
        self.claimed_workspace_keys
            .insert(retry.workspace_key.clone());
    }

    pub(crate) fn release_tracker_issue_id_if_unowned(&mut self, issue_id: &str) {
        let owned_by_running = self
            .running
            .values()
            .any(|entry| entry.issue.id == issue_id);
        let owned_by_retry = self
            .retry_attempts
            .values()
            .any(|retry| retry.issue_id == issue_id);
        if !owned_by_running && !owned_by_retry {
            self.claimed_issue_ids.remove(issue_id);
        }
    }

    pub(crate) fn release_workspace_key_if_unowned(&mut self, workspace_key: &str) {
        let owned_by_running = self
            .running
            .values()
            .any(|entry| entry.workspace_key == workspace_key);
        let owned_by_retry = self
            .retry_attempts
            .values()
            .any(|retry| retry.workspace_key == workspace_key);
        if !owned_by_running && !owned_by_retry {
            self.claimed_workspace_keys.remove(workspace_key);
        }
    }
}
fn bounded_recent_event_message(message: &str) -> String {
    if message.len() <= RECENT_EVENT_MESSAGE_LIMIT_BYTES {
        return message.to_string();
    }

    let mut end = RECENT_EVENT_MESSAGE_LIMIT_BYTES - "...".len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

pub fn source_issue_key(source_id: &str, issue_id: &str) -> String {
    if source_id == DEFAULT_SOURCE_ID {
        issue_id.to_string()
    } else {
        format!("{}:{}{}", source_id.len(), source_id, issue_id)
    }
}

pub fn source_workspace_key(source_id: &str, identifier: &str) -> String {
    workspace_source_workspace_key(source_id, identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str, identifier: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: format!("Issue {identifier}"),
            description: None,
            priority: None,
            state: "In Progress".to_string(),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn releases_claimed_workspace_key_after_retry_entry_was_removed() {
        let mut state = OrchestratorState::default();
        let retrying = issue("id-1", "S/001");
        let retry = state.schedule_retry_now(&retrying, 1, Some("transient failure".to_string()));
        state.retry_attempts.remove(&retrying.id);

        state.release(&retrying.id);
        assert!(state.claimed_workspace_keys.contains(&retry.workspace_key));

        state.release_workspace_key_if_unowned(&retry.workspace_key);
        assert!(!state.claimed_workspace_keys.contains(&retry.workspace_key));
    }

    #[test]
    fn running_snapshot_retains_ssh_target_and_workspace_path() {
        let mut state = OrchestratorState::default();
        let issue = issue("id-ssh", "S-SSH");
        let path = std::path::PathBuf::from("/remote/work/S-SSH");
        state.claim_running_on_target_for_source(
            DEFAULT_SOURCE_ID,
            issue,
            Some(2),
            ExecutionTarget::Ssh {
                host: "worker-a".to_string(),
            },
            path.clone(),
            chrono::Utc::now(),
        );

        let snapshot = state.snapshot(chrono::Utc::now());
        assert_eq!(snapshot.running[0].workspace_path, path);
        assert_eq!(
            snapshot.running[0].execution_target,
            ExecutionTarget::Ssh {
                host: "worker-a".to_string()
            }
        );
    }
}
