use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::agent::codex::CodexAppServerClient;
use crate::agent::runner::{AgentRunner, SymphonyAgentRunner, WorkerOutcome};
use crate::config::{ConfigReloader, ConfigSetReloader, EffectiveConfig};
use crate::domain::{CodexEvent, Issue, WorkerExitReason};
use crate::error::Result;
use crate::orchestrator::state::source_issue_key;
use crate::orchestrator::{OrchestratorState, is_dispatch_eligible_for_source};
use crate::time::{ms_from_now, now_utc, system_monotonic_ms};
use crate::tracker::TrackerClient;
use crate::tracker::github::GitHubTrackerClient;
use crate::workspace::WorkspaceManager;

struct SourceRun {
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    candidates: Vec<Issue>,
}

pub async fn run_service_until_shutdown(
    reloader: ConfigReloader,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    run_multi_source_service_until_shutdown(ConfigSetReloader::from_single(reloader), shutdown)
        .await
}

pub async fn run_multi_source_service_until_shutdown(
    mut reloaders: ConfigSetReloader,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<CodexEvent>();
    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<WorkerOutcome>();
    let mut state = OrchestratorState::default();

    for config in reloaders.current() {
        startup_terminal_cleanup(config).await;
    }
    tokio::pin!(shutdown);

    loop {
        let configs = reloaders.current_cloned();
        drain_events(&mut state, &mut event_rx);
        drain_outcomes(&mut state, &configs, &mut outcome_rx);
        for (source_id, result) in reloaders.reload_if_changed() {
            if let Err(error) = result {
                warn!(source_id = %source_id, error = %error, "workflow_reload_failed keeping_last_good=true");
            }
        }
        tick(
            &mut state,
            reloaders.current_cloned(),
            event_tx.clone(),
            outcome_tx.clone(),
        )
        .await;

        let delay = Duration::from_millis(reloaders.poll_interval_ms());
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
            warn!(source_id = %config.source.id, error = %error, "startup_cleanup_tracker_unavailable");
            return;
        }
    };
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "startup_cleanup_workspace_unavailable");
            return;
        }
    };
    match tracker
        .fetch_issues_by_states(&config.tracker.terminal_states)
        .await
    {
        Ok(issues) => {
            for issue in issues {
                if let Err(error) = workspace
                    .remove_for_source_issue(&config.source.id, &issue)
                    .await
                {
                    warn!(source_id = %config.source.id, issue_id = %issue.id, issue_identifier = %issue.identifier, error = %error, "startup_cleanup_failed");
                }
            }
        }
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "startup_cleanup_fetch_failed")
        }
    }
}

async fn tick(
    state: &mut OrchestratorState,
    configs: Vec<EffectiveConfig>,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerOutcome>,
) {
    let mut runs = Vec::new();
    for config in configs {
        reconcile_stalled(state, &config);
        if let Err(error) = config.validate_dispatch() {
            warn!(source_id = %config.source.id, error = %error, "dispatch_validation_failed");
            continue;
        }
        let tracker = match GitHubTrackerClient::new(&config) {
            Ok(tracker) => Arc::new(tracker),
            Err(error) => {
                warn!(source_id = %config.source.id, error = %error, "tracker_create_failed");
                continue;
            }
        };
        reconcile_tracker_states(state, &config, tracker.as_ref()).await;
        let candidates = match tracker.fetch_candidate_issues().await {
            Ok(issues) => issues,
            Err(error) => {
                warn!(source_id = %config.source.id, error = %error, "candidate_fetch_failed");
                reschedule_due_retries_after_fetch_error(state, &config);
                continue;
            }
        };
        runs.push(SourceRun {
            config,
            tracker,
            candidates,
        });
    }

    let global_agent_limit = runs
        .iter()
        .map(|run| run.config.agent.max_concurrent_agents)
        .min()
        .unwrap_or(0);

    let mut dispatch_candidates = Vec::new();
    for (source_index, run) in runs.iter().enumerate() {
        for issue in &run.candidates {
            dispatch_candidates.push((source_index, issue.clone()));
        }
    }
    dispatch_candidates.sort_by(|(left_source, left), (right_source, right)| {
        compare_source_issue(
            &runs[*left_source].config.source.id,
            left,
            &runs[*right_source].config.source.id,
            right,
        )
    });

    for (source_index, issue) in dispatch_candidates {
        if state.running.len() >= global_agent_limit {
            break;
        }
        let run = &runs[source_index];
        if !is_dispatch_eligible_for_source(&run.config.source.id, &issue, state, &run.config) {
            continue;
        }
        dispatch_issue(
            state,
            run.config.clone(),
            run.tracker.clone(),
            issue,
            None,
            event_tx.clone(),
            outcome_tx.clone(),
        )
        .await;
    }

    for run in runs {
        dispatch_due_retries(
            state,
            run.config,
            run.tracker,
            run.candidates,
            global_agent_limit,
            event_tx.clone(),
            outcome_tx.clone(),
        )
        .await;
    }
}

fn compare_source_issue(
    left_source_id: &str,
    left: &Issue,
    right_source_id: &str,
    right: &Issue,
) -> Ordering {
    compare_priority(left, right)
        .then_with(|| compare_created_at(left, right))
        .then_with(|| left_source_id.cmp(right_source_id))
        .then_with(|| left.identifier.cmp(&right.identifier))
}

fn compare_priority(left: &Issue, right: &Issue) -> Ordering {
    match (left.priority, right.priority) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_created_at(left: &Issue, right: &Issue) -> Ordering {
    match (left.created_at, right.created_at) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn reconcile_stalled(state: &mut OrchestratorState, config: &EffectiveConfig) {
    let now = now_utc();
    let now_ms = system_monotonic_ms();
    for issue_id in state.stalled_issue_ids_for_source(&config.source.id, config, now) {
        state.worker_exit_for_source(
            &config.source.id,
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
    let ids = state.running_issue_ids_for_source(&config.source.id);
    if ids.is_empty() {
        return;
    }
    let refreshed = match tracker.fetch_issue_states_by_ids(&ids).await {
        Ok(issues) => issues,
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "reconcile_refresh_failed");
            return;
        }
    };
    let workspace = WorkspaceManager::new(&config.workspace, config.hooks.clone()).ok();
    for issue in refreshed {
        if config.is_terminal_state(&issue.state) {
            state.release_for_source(&config.source.id, &issue.id);
            if let Some(workspace) = &workspace
                && let Err(error) = workspace
                    .remove_for_source_issue(&config.source.id, &issue)
                    .await
            {
                warn!(source_id = %config.source.id, issue_id = %issue.id, issue_identifier = %issue.identifier, error = %error, "terminal_cleanup_failed");
            }
        } else if config.is_active_state(&issue.state) {
            if let Some(entry) = state.running_entry_mut_for_source(&config.source.id, &issue.id) {
                entry.identifier = issue.identifier.clone();
                entry.issue = issue;
            }
        } else {
            state.release_for_source(&config.source.id, &issue.id);
        }
    }
}

fn reschedule_due_retries_after_fetch_error(
    state: &mut OrchestratorState,
    config: &EffectiveConfig,
) {
    let now = system_monotonic_ms();
    for issue_key in state.due_retry_keys_for_source(&config.source.id, now) {
        if let Some(retry) = state.retry_attempts.get_mut(&issue_key) {
            retry.attempt = retry.attempt.saturating_add(1);
            retry.error = Some("retry poll failed".to_string());
            retry.due_at_ms = ms_from_now(crate::orchestrator::failure_retry_delay_ms(
                retry.attempt,
                config.agent.max_retry_backoff_ms,
            ));
        }
    }
}

async fn dispatch_due_retries(
    state: &mut OrchestratorState,
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    candidates: Vec<Issue>,
    global_agent_limit: usize,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerOutcome>,
) {
    let now = system_monotonic_ms();
    let due_keys = state.due_retry_keys_for_source(&config.source.id, now);
    for issue_key in due_keys {
        let Some(mut retry) = state.retry_attempts.remove(&issue_key) else {
            continue;
        };
        state.release_retry_claim(&retry);
        let Some(issue) = candidates
            .iter()
            .find(|issue| issue.id == retry.issue_id)
            .cloned()
        else {
            continue;
        };
        if state.running.len() >= global_agent_limit {
            retry.attempt = retry.attempt.saturating_add(1);
            retry.error = Some("no available orchestrator slots".to_string());
            retry.due_at_ms = ms_from_now(crate::orchestrator::failure_retry_delay_ms(
                retry.attempt,
                config.agent.max_retry_backoff_ms,
            ));
            state.requeue_retry(retry);
            continue;
        }
        if is_dispatch_eligible_for_source(&config.source.id, &issue, state, &config) {
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
            retry.attempt = retry.attempt.saturating_add(1);
            retry.error = Some("no available orchestrator slots".to_string());
            retry.due_at_ms = ms_from_now(crate::orchestrator::failure_retry_delay_ms(
                retry.attempt,
                config.agent.max_retry_backoff_ms,
            ));
            state.requeue_retry(retry);
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
    let source_id = config.source.id.clone();
    let issue_key = source_issue_key(&source_id, &issue.id);
    state.claim_running_for_source(&source_id, issue.clone(), attempt, started_at);
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            state.worker_exit_for_source(
                &source_id,
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
    tokio::spawn(async move {
        let event_issue_key = issue_key.clone();
        let outcome_issue_key = issue_key;
        let callback_tx = event_tx.clone();
        let raw_issue_id = issue.id.clone();
        let mut outcome = runner
            .run(
                issue,
                attempt,
                Box::new(move |mut event| {
                    event.issue_id = event_issue_key.clone();
                    let _ = callback_tx.send(event);
                }),
            )
            .await
            .unwrap_or_else(|error| WorkerOutcome {
                issue_id: raw_issue_id,
                reason: WorkerExitReason::Failed(error.to_string()),
            });
        outcome.issue_id = outcome_issue_key;
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
    configs: &[EffectiveConfig],
    rx: &mut mpsc::UnboundedReceiver<WorkerOutcome>,
) {
    let now = now_utc();
    let now_ms = system_monotonic_ms();
    while let Ok(outcome) = rx.try_recv() {
        let source_id = state
            .running
            .get(&outcome.issue_id)
            .map(|entry| entry.source_id.clone());
        let Some(config) = source_id
            .as_deref()
            .and_then(|source_id| configs.iter().find(|config| config.source.id == source_id))
        else {
            warn!(issue_key = %outcome.issue_id, "worker_outcome_without_running_entry");
            continue;
        };
        state.worker_exit_by_key(&outcome.issue_id, outcome.reason, config, now_ms, now);
    }
}
