use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::{DEFAULT_SOURCE_ID, EffectiveConfig, normalize_state};
use crate::domain::{
    CodexEvent, Issue, LiveSession, RetryEntry, RunningSnapshot, RuntimeSnapshot, StateCounts,
    TokenTotals, WorkerExitReason,
};
use crate::orchestrator::retry::{
    continuation_retry_due_at_ms, failure_retry_delay_ms, failure_retry_due_at_ms,
};
use crate::time::{ms_from_now, utc_elapsed_ms};
use crate::workspace::sanitize_workspace_key;

#[derive(Clone, Debug)]
pub struct RunningEntry {
    pub issue: Issue,
    pub source_id: String,
    pub identifier: String,
    pub workspace_key: String,
    pub started_at: DateTime<Utc>,
    pub retry_attempt: Option<u32>,
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

    pub fn apply_codex_event(&mut self, event: CodexEvent) {
        if let Some(entry) = self.running.get_mut(&event.issue_id) {
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
        let running = self
            .running
            .values()
            .map(|entry| RunningSnapshot {
                source_id: entry.source_id.clone(),
                issue_id: entry.issue.id.clone(),
                issue_identifier: entry.identifier.clone(),
                state: entry.issue.state.clone(),
                session_id: entry
                    .live_session
                    .as_ref()
                    .map(|session| session.session_id.clone()),
                turn_count: entry
                    .live_session
                    .as_ref()
                    .map(|session| session.turn_count)
                    .unwrap_or(0),
                started_at: entry.started_at,
            })
            .collect();
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
            running,
            retrying: self.retry_attempts.values().cloned().collect(),
            codex_totals: self.codex_totals.clone(),
            seconds_running: self.ended_runtime_seconds + active_seconds,
            rate_limits: self.codex_rate_limits.clone(),
        }
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

pub fn source_issue_key(source_id: &str, issue_id: &str) -> String {
    if source_id == DEFAULT_SOURCE_ID {
        issue_id.to_string()
    } else {
        format!("{}:{}{}", source_id.len(), source_id, issue_id)
    }
}

pub fn source_workspace_key(source_id: &str, identifier: &str) -> String {
    let issue_key = sanitize_workspace_key(identifier);
    if source_id == DEFAULT_SOURCE_ID {
        issue_key
    } else {
        format!("{}/{}", sanitize_workspace_key(source_id), issue_key)
    }
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
}
