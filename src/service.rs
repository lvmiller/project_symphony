use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::agent::codex::CodexAppServerClient;
use crate::agent::runner::{AgentRunner, SymphonyAgentRunner, WorkerOutcome};
use crate::config::{ConfigReloader, EffectiveConfig};
use crate::domain::{CodexEvent, Issue, WorkerExitReason};
use crate::error::Result;
use crate::orchestrator::{OrchestratorState, is_dispatch_eligible, sort_for_dispatch};
use crate::time::{ms_from_now, now_utc, system_monotonic_ms};
use crate::tracker::TrackerClient;
use crate::tracker::github::GitHubTrackerClient;
use crate::workspace::WorkspaceManager;

pub async fn run_service_until_shutdown(
    mut reloader: ConfigReloader,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<CodexEvent>();
    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<WorkerOutcome>();
    let mut state = OrchestratorState::default();

    startup_terminal_cleanup(reloader.current()).await;
    tokio::pin!(shutdown);

    loop {
        drain_events(&mut state, &mut event_rx);
        drain_outcomes(&mut state, reloader.current(), &mut outcome_rx);
        if let Err(error) = reloader.reload_if_changed() {
            warn!(error = %error, "workflow_reload_failed keeping_last_good=true");
        }
        tick(
            &mut state,
            reloader.current().clone(),
            event_tx.clone(),
            outcome_tx.clone(),
        )
        .await;

        let delay = Duration::from_millis(reloader.current().polling.interval_ms);
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown_requested");
                return Ok(());
            }
            _ = sleep(delay) => {}
        }
    }
}

async fn startup_terminal_cleanup(config: &EffectiveConfig) {
    let tracker = match GitHubTrackerClient::new(config) {
        Ok(tracker) => tracker,
        Err(error) => {
            warn!(error = %error, "startup_cleanup_tracker_unavailable");
            return;
        }
    };
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            warn!(error = %error, "startup_cleanup_workspace_unavailable");
            return;
        }
    };
    match tracker
        .fetch_issues_by_states(&config.tracker.terminal_states)
        .await
    {
        Ok(issues) => {
            for issue in issues {
                if let Err(error) = workspace.remove_for_issue(&issue).await {
                    warn!(issue_id = %issue.id, issue_identifier = %issue.identifier, error = %error, "startup_cleanup_failed");
                }
            }
        }
        Err(error) => warn!(error = %error, "startup_cleanup_fetch_failed"),
    }
}

async fn tick(
    state: &mut OrchestratorState,
    config: EffectiveConfig,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerOutcome>,
) {
    reconcile_stalled(state, &config);
    if let Err(error) = config.validate_dispatch() {
        warn!(error = %error, "dispatch_validation_failed");
        return;
    }
    let tracker = match GitHubTrackerClient::new(&config) {
        Ok(tracker) => Arc::new(tracker),
        Err(error) => {
            warn!(error = %error, "tracker_create_failed");
            return;
        }
    };
    reconcile_tracker_states(state, &config, tracker.as_ref()).await;
    let mut issues = match tracker.fetch_candidate_issues().await {
        Ok(issues) => issues,
        Err(error) => {
            warn!(error = %error, "candidate_fetch_failed");
            return;
        }
    };
    sort_for_dispatch(&mut issues);
    for issue in issues {
        if !is_dispatch_eligible(&issue, state, &config) {
            continue;
        }
        dispatch_issue(
            state,
            config.clone(),
            tracker.clone(),
            issue,
            None,
            event_tx.clone(),
            outcome_tx.clone(),
        )
        .await;
    }
    dispatch_due_retries(state, config, tracker, event_tx, outcome_tx).await;
}

fn reconcile_stalled(state: &mut OrchestratorState, config: &EffectiveConfig) {
    let now = now_utc();
    let now_ms = system_monotonic_ms();
    for issue_id in state.stalled_issue_ids(config, now) {
        state.worker_exit(
            &issue_id,
            WorkerExitReason::Stalled("codex stalled".to_string()),
            config,
            now_ms,
            now,
        );
    }
}

async fn reconcile_tracker_states(
    state: &mut OrchestratorState,
    config: &EffectiveConfig,
    tracker: &dyn TrackerClient,
) {
    if state.running.is_empty() {
        return;
    }
    let ids: Vec<String> = state.running.keys().cloned().collect();
    let refreshed = match tracker.fetch_issue_states_by_ids(&ids).await {
        Ok(issues) => issues,
        Err(error) => {
            warn!(error = %error, "reconcile_refresh_failed");
            return;
        }
    };
    let workspace = WorkspaceManager::new(&config.workspace, config.hooks.clone()).ok();
    for issue in refreshed {
        if config.is_terminal_state(&issue.state) {
            state.release(&issue.id);
            if let Some(workspace) = &workspace
                && let Err(error) = workspace.remove_for_issue(&issue).await
            {
                warn!(issue_id = %issue.id, issue_identifier = %issue.identifier, error = %error, "terminal_cleanup_failed");
            }
        } else if config.is_active_state(&issue.state) {
            if let Some(entry) = state.running.get_mut(&issue.id) {
                entry.issue = issue;
            }
        } else {
            state.release(&issue.id);
        }
    }
}

async fn dispatch_due_retries(
    state: &mut OrchestratorState,
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerOutcome>,
) {
    let now = system_monotonic_ms();
    let due_ids: Vec<String> = state
        .retry_attempts
        .iter()
        .filter(|(_, retry)| retry.due_at_ms <= now)
        .map(|(id, _)| id.clone())
        .collect();
    if due_ids.is_empty() {
        return;
    }
    let candidates = match tracker.fetch_candidate_issues().await {
        Ok(candidates) => candidates,
        Err(error) => {
            warn!(error = %error, "retry_candidate_fetch_failed");
            for issue_id in due_ids {
                if let Some(retry) = state.retry_attempts.get_mut(&issue_id) {
                    retry.attempt = retry.attempt.saturating_add(1);
                    retry.error = Some("retry poll failed".to_string());
                    retry.due_at_ms = ms_from_now(crate::orchestrator::failure_retry_delay_ms(
                        retry.attempt,
                        config.agent.max_retry_backoff_ms,
                    ));
                }
            }
            return;
        }
    };
    for issue_id in due_ids {
        let Some(retry) = state.retry_attempts.remove(&issue_id) else {
            continue;
        };
        let Some(issue) = candidates
            .iter()
            .find(|issue| issue.id == issue_id)
            .cloned()
        else {
            state.release(&issue_id);
            continue;
        };
        state.claimed.remove(&issue_id);
        state.claimed_workspace_keys.remove(&retry.workspace_key);
        if is_dispatch_eligible(&issue, state, &config) {
            dispatch_issue(
                state,
                config.clone(),
                tracker.clone(),
                issue,
                Some(retry.attempt),
                event_tx.clone(),
                outcome_tx.clone(),
            )
            .await;
        } else {
            let mut retry = retry;
            retry.attempt = retry.attempt.saturating_add(1);
            retry.error = Some("no available orchestrator slots".to_string());
            retry.due_at_ms = ms_from_now(crate::orchestrator::failure_retry_delay_ms(
                retry.attempt,
                config.agent.max_retry_backoff_ms,
            ));
            state.claimed.insert(issue_id.clone());
            state
                .claimed_workspace_keys
                .insert(retry.workspace_key.clone());
            state.retry_attempts.insert(issue_id, retry);
        }
    }
}

async fn dispatch_issue(
    state: &mut OrchestratorState,
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    issue: Issue,
    attempt: Option<u32>,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerOutcome>,
) {
    let started_at = now_utc();
    state.claim_running(issue.clone(), attempt, started_at);
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            state.worker_exit(
                &issue.id,
                WorkerExitReason::Failed(error.to_string()),
                &config,
                system_monotonic_ms(),
                now_utc(),
            );
            return;
        }
    };
    let codex = Arc::new(CodexAppServerClient::new(config.codex.clone()));
    let runner = SymphonyAgentRunner::new(config, workspace, tracker, codex);
    let issue_id = issue.id.clone();
    tokio::spawn(async move {
        let event_issue_id = issue_id.clone();
        let callback_tx = event_tx.clone();
        let outcome = runner
            .run(
                issue,
                attempt,
                Box::new(move |mut event| {
                    if event.issue_id.is_empty() {
                        event.issue_id = event_issue_id.clone();
                    }
                    let _ = callback_tx.send(event);
                }),
            )
            .await
            .unwrap_or_else(|error| WorkerOutcome {
                issue_id,
                reason: WorkerExitReason::Failed(error.to_string()),
            });
        let _ = outcome_tx.send(outcome);
    });
}

fn drain_events(state: &mut OrchestratorState, rx: &mut mpsc::UnboundedReceiver<CodexEvent>) {
    while let Ok(event) = rx.try_recv() {
        state.apply_codex_event(event);
    }
}

fn drain_outcomes(
    state: &mut OrchestratorState,
    config: &EffectiveConfig,
    rx: &mut mpsc::UnboundedReceiver<WorkerOutcome>,
) {
    let now = now_utc();
    let now_ms = system_monotonic_ms();
    while let Ok(outcome) = rx.try_recv() {
        state.worker_exit(&outcome.issue_id, outcome.reason, config, now_ms, now);
    }
}
