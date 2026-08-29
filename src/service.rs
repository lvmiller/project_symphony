use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::agent::codex::CodexAppServerClient;
use crate::agent::runner::{AgentRunner, SymphonyAgentRunner, WorkerOutcome};
use crate::config::{
    ConfigReloader, ConfigSetReloader, EffectiveConfig, config_reload_error_class,
};
use crate::domain::{CodexEvent, ExecutionTarget, Issue, WorkerExitReason};
use crate::error::Result;
use crate::observability::http::{SharedStatus, spawn_http_server};
use crate::orchestrator::state::{ReconcileDecision, source_issue_key};
use crate::orchestrator::{OrchestratorState, is_dispatch_eligible_for_source};
use crate::time::{ms_from_now, now_utc, system_monotonic_ms};
use crate::tracker::github::{GitHubGraphqlExecutor, GitHubTrackerClient};
use crate::tracker::{TrackerClient, TrackerWriter};
use crate::workspace::{WorkspaceManager, source_workspace_key, source_workspace_namespace};

struct WorkerEvent {
    issue_key: String,
    generation: u64,
    event: CodexEvent,
}

struct WorkerResult {
    issue_key: String,
    generation: u64,
    outcome: WorkerOutcome,
}

#[derive(Clone)]
struct WorkerChannels {
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    outcome_tx: mpsc::UnboundedSender<WorkerResult>,
}

struct WorkerTask {
    generation: u64,
    target: ExecutionTarget,
    handle: JoinHandle<()>,
}

struct SourceRun {
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    candidates: Vec<Issue>,
}

struct DispatchRequest {
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    issue: Issue,
    attempt: Option<u32>,
    target: ExecutionTarget,
}

#[derive(Default)]
struct WorkerRegistry {
    next_generation: u64,
    tasks: BTreeMap<String, WorkerTask>,
}

impl WorkerRegistry {
    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("worker dispatch generation exhausted");
        self.next_generation
    }

    fn contains(&self, issue_key: &str) -> bool {
        self.tasks.contains_key(issue_key)
    }

    fn matches(&self, issue_key: &str, generation: u64) -> bool {
        self.tasks
            .get(issue_key)
            .is_some_and(|task| task.generation == generation)
    }

    fn active_on_ssh_host(&self, host: &str) -> usize {
        self.tasks
            .values()
            .filter(|task| {
                task.target
                    .host()
                    .is_some_and(|active_host| active_host.eq_ignore_ascii_case(host))
            })
            .count()
    }

    fn insert(
        &mut self,
        issue_key: String,
        generation: u64,
        target: ExecutionTarget,
        handle: JoinHandle<()>,
    ) {
        assert!(
            self.tasks
                .insert(
                    issue_key,
                    WorkerTask {
                        generation,
                        target,
                        handle,
                    },
                )
                .is_none(),
            "worker task already registered"
        );
    }
}

fn worker_dispatch_permitted(workers: &WorkerRegistry, issue_key: &str) -> bool {
    !workers.contains(issue_key)
}

fn select_execution_target(
    workers: &WorkerRegistry,
    config: &EffectiveConfig,
) -> Option<ExecutionTarget> {
    if config.worker.ssh_hosts.is_empty() {
        return Some(ExecutionTarget::Local);
    }

    config.worker.ssh_hosts.iter().find_map(|host| {
        (workers.active_on_ssh_host(host) < config.worker.max_concurrent_agents_per_host)
            .then(|| ExecutionTarget::Ssh { host: host.clone() })
    })
}

pub async fn run_service_until_shutdown(
    reloader: ConfigReloader,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    run_multi_source_service_until_shutdown(
        ConfigSetReloader::from_single(reloader),
        shutdown,
        None,
    )
    .await
}

pub async fn run_multi_source_service_until_shutdown(
    mut reloaders: ConfigSetReloader,
    shutdown: impl std::future::Future<Output = ()>,
    server_bind: Option<SocketAddr>,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<WorkerResult>();
    let channels = WorkerChannels {
        event_tx,
        outcome_tx,
    };
    let mut workers = WorkerRegistry::default();
    let mut state = OrchestratorState::default();
    let initial_configs = reloaders.current_cloned();
    let shared_status = SharedStatus::new(&initial_configs);
    let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel::<()>();
    let refresh_pending = Arc::new(AtomicBool::new(false));
    let http_server = if let Some(bind_addr) = server_bind {
        let server = spawn_http_server(
            bind_addr,
            shared_status.clone(),
            refresh_tx.clone(),
            refresh_pending.clone(),
        )
        .await?;
        info!(bind_addr = %server.local_addr, "http_server_started");
        Some(server)
    } else {
        None
    };

    for config in reloaders.current() {
        startup_terminal_cleanup(config).await;
    }
    shared_status
        .publish(&state, &reloaders.current_cloned())
        .await;
    tokio::pin!(shutdown);
    let mut startup_retention_pending = true;

    let result = loop {
        for (source_id, workflow_path, result) in reloaders.reload_if_changed() {
            let workflow_path = workflow_path.display().to_string();
            match result {
                Ok(true) => {
                    info!(
                        source_id = %source_id,
                        workflow_path = %workflow_path,
                        last_known_good_active = true,
                        "workflow_reload_succeeded"
                    );
                }
                Ok(false) => {}
                Err(error) => {
                    warn!(
                        source_id = %source_id,
                        workflow_path = %workflow_path,
                        error_class = config_reload_error_class(&error),
                        last_known_good_active = true,
                        "workflow_reload_failed"
                    );
                }
            }
        }
        let configs = reloaders.current_cloned();
        tick(&mut state, &mut workers, configs.clone(), channels.clone()).await;
        if startup_retention_pending {
            for config in &configs {
                let source_namespace_segments = configs
                    .iter()
                    .filter(|other| {
                        other.workspace.root == config.workspace.root
                            && other.source.id != crate::config::DEFAULT_SOURCE_ID
                    })
                    .map(|other| source_workspace_namespace(&other.source.id))
                    .collect();
                startup_orphan_workspace_pruning(config, &state, &source_namespace_segments).await;
            }
            startup_retention_pending = false;
        }
        shared_status
            .publish(&state, &reloaders.current_cloned())
            .await;

        let poll_delay = Duration::from_millis(reloaders.poll_interval_ms());
        let retry_delay = next_retry_delay(&state);
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown_requested");
                break Ok(());
            }
            event = event_rx.recv() => {
                if let Some(event) = event {
                    apply_worker_event(&mut state, &workers, event);
                    shared_status
                        .publish(&state, &reloaders.current_cloned())
                        .await;
                }
            }
            outcome = outcome_rx.recv() => {
                if let Some(outcome) = outcome {
                    apply_worker_outcome(
                        &mut state,
                        &reloaders.current_cloned(),
                        &mut workers,
                        outcome,
                    )
                    .await;
                    shared_status
                        .publish(&state, &reloaders.current_cloned())
                        .await;
                }
            }
            refresh = refresh_rx.recv() => {
                if refresh.is_some() {
                    while refresh_rx.try_recv().is_ok() {}
                    refresh_pending.store(false, AtomicOrdering::Release);
                }
            }
            _ = sleep(poll_delay) => {}
            _ = sleep(retry_delay) => {}
        }
    };
    shutdown_workers(&mut workers).await;
    if let Some(server) = http_server {
        server.task.abort();
        let _ = server.task.await;
    }
    result
}

fn next_retry_delay(state: &OrchestratorState) -> Duration {
    state
        .next_retry_due_at_ms()
        .map(|due_at_ms| Duration::from_millis(due_at_ms.saturating_sub(system_monotonic_ms())))
        .unwrap_or(Duration::from_secs(24 * 60 * 60))
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

async fn startup_orphan_workspace_pruning(
    config: &EffectiveConfig,
    state: &OrchestratorState,
    source_namespace_segments: &BTreeSet<String>,
) {
    let Some(max_age_days) = config.workspace.retention.max_age_days else {
        return;
    };
    let tracker = match GitHubTrackerClient::new(config) {
        Ok(tracker) => tracker,
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "startup_retention_tracker_unavailable");
            return;
        }
    };
    let active_issues = match tracker
        .fetch_issues_by_states(&config.tracker.active_states)
        .await
    {
        Ok(issues) => issues,
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "startup_retention_fetch_failed");
            return;
        }
    };
    let mut protected_workspace_keys: BTreeSet<String> = state
        .claimed_workspace_keys
        .iter()
        .cloned()
        .chain(
            state
                .running
                .values()
                .filter(|entry| entry.source_id == config.source.id)
                .map(|entry| entry.workspace_key.clone()),
        )
        .chain(
            state
                .retry_attempts
                .values()
                .filter(|retry| retry.source_id == config.source.id)
                .map(|retry| retry.workspace_key.clone()),
        )
        .collect();
    protected_workspace_keys.extend(
        active_issues
            .iter()
            .map(|issue| source_workspace_key(&config.source.id, &issue.identifier)),
    );
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            warn!(source_id = %config.source.id, error = %error, "startup_retention_workspace_unavailable");
            return;
        }
    };
    if let Err(error) = workspace
        .prune_orphaned_workspaces_for_source_with_namespaces(
            &config.source.id,
            &protected_workspace_keys,
            source_namespace_segments,
            max_age_days,
        )
        .await
    {
        warn!(source_id = %config.source.id, error = %error, "startup_retention_pruning_failed");
    }
}

async fn tick(
    state: &mut OrchestratorState,
    workers: &mut WorkerRegistry,
    configs: Vec<EffectiveConfig>,
    channels: WorkerChannels,
) {
    let mut runs = Vec::new();
    for config in configs {
        reconcile_stalled(state, workers, &config).await;
        let tracker = match GitHubTrackerClient::new(&config) {
            Ok(tracker) => Arc::new(tracker),
            Err(error) => {
                warn!(source_id = %config.source.id, error = %error, "tracker_create_failed");
                continue;
            }
        };
        reconcile_tracker_states(state, workers, &config, tracker.as_ref()).await;
        if let Err(error) = config.validate_dispatch() {
            warn!(source_id = %config.source.id, error = %error, "dispatch_validation_failed");
            continue;
        }
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
        let Some(target) = select_execution_target(workers, &run.config) else {
            continue;
        };
        dispatch_issue(
            state,
            workers,
            DispatchRequest {
                config: run.config.clone(),
                tracker: run.tracker.clone(),
                issue,
                attempt: None,
                target,
            },
            channels.clone(),
        )
        .await;
    }

    for run in runs {
        dispatch_due_retries(
            state,
            workers,
            run.config,
            run.tracker,
            run.candidates,
            global_agent_limit,
            channels.clone(),
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

async fn reconcile_stalled(
    state: &mut OrchestratorState,
    workers: &mut WorkerRegistry,
    config: &EffectiveConfig,
) {
    let now = now_utc();
    let now_ms = system_monotonic_ms();
    for issue_id in state.stalled_issue_ids_for_source(&config.source.id, config, now) {
        let issue_key = source_issue_key(&config.source.id, &issue_id);
        abort_worker(workers, &issue_key).await;
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
    workers: &mut WorkerRegistry,
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
    for issue_id in ids {
        let latest = refreshed.iter().find(|issue| issue.id == issue_id).cloned();
        let decision = state.reconcile_running_issue_for_source(
            &config.source.id,
            &issue_id,
            latest.as_ref(),
            config,
        );
        let issue_key = source_issue_key(&config.source.id, &issue_id);
        match decision {
            ReconcileDecision::CancelTerminal => {
                let issue = latest.expect("terminal reconciliation requires a tracker issue");
                let target = state
                    .running
                    .get(&issue_key)
                    .map(|entry| entry.execution_target.clone())
                    .unwrap_or_default();
                abort_worker(workers, &issue_key).await;
                state.release_for_source(&config.source.id, &issue_id);
                if let Some(workspace) = &workspace
                    && let Err(error) = workspace
                        .remove_for_target(&target, &config.source.id, &issue)
                        .await
                {
                    warn!(source_id = %config.source.id, issue_id = %issue.id, issue_identifier = %issue.identifier, execution_target = ?target, error = %error, "terminal_cleanup_failed");
                }
            }
            ReconcileDecision::CancelNonActive | ReconcileDecision::MissingFromTracker => {
                abort_worker(workers, &issue_key).await;
                state.release_for_source(&config.source.id, &issue_id);
            }
            ReconcileDecision::NoRunningEntry | ReconcileDecision::RefreshedActive => {}
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
    workers: &mut WorkerRegistry,
    config: EffectiveConfig,
    tracker: Arc<GitHubTrackerClient>,
    candidates: Vec<Issue>,
    global_agent_limit: usize,
    channels: WorkerChannels,
) {
    let now = system_monotonic_ms();
    let due_keys = state.due_retry_keys_for_source(&config.source.id, now);
    for issue_key in due_keys {
        if !worker_dispatch_permitted(workers, &issue_key) {
            continue;
        }
        let Some(retry) = state.retry_attempts.remove(&issue_key) else {
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
            state.requeue_retry(retry);
            continue;
        }
        if !is_dispatch_eligible_for_source(&config.source.id, &issue, state, &config) {
            state.requeue_retry(retry);
            continue;
        }

        // A normal worker exit is a continuation of the same attempt, so it
        // retains both the original host and that host-local workspace. A
        // failed worker is already terminated before this new dispatch and
        // therefore may receive a fresh target selection.
        let target = if retry.error.is_none() {
            let available = match &retry.execution_target {
                ExecutionTarget::Local => true,
                ExecutionTarget::Ssh { host } => {
                    workers.active_on_ssh_host(host) < config.worker.max_concurrent_agents_per_host
                }
            };
            available.then(|| retry.execution_target.clone())
        } else {
            select_execution_target(workers, &config)
        };
        let Some(target) = target else {
            state.requeue_retry(retry);
            continue;
        };

        dispatch_issue(
            state,
            workers,
            DispatchRequest {
                config: config.clone(),
                tracker: tracker.clone(),
                issue,
                attempt: Some(retry.attempt),
                target,
            },
            channels.clone(),
        )
        .await;
    }
}

async fn dispatch_issue(
    state: &mut OrchestratorState,
    workers: &mut WorkerRegistry,
    request: DispatchRequest,
    channels: WorkerChannels,
) {
    let DispatchRequest {
        config,
        tracker,
        issue,
        attempt,
        target,
    } = request;
    let started_at = now_utc();
    let source_id = config.source.id.clone();
    let issue_key = source_issue_key(&source_id, &issue.id);
    if !worker_dispatch_permitted(workers, &issue_key) {
        warn!(issue_key = %issue_key, "worker_dispatch_blocked_existing_task");
        return;
    }
    let workspace = match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
        Ok(workspace) => workspace,
        Err(error) => {
            state.claim_running_on_target_for_source(
                &source_id,
                issue.clone(),
                attempt,
                target,
                std::path::PathBuf::new(),
                started_at,
            );
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
    let workspace_path =
        match workspace.workspace_path_for_source_identifier(&source_id, &issue.identifier) {
            Ok((_, path)) => path,
            Err(error) => {
                state.claim_running_on_target_for_source(
                    &source_id,
                    issue.clone(),
                    attempt,
                    target,
                    std::path::PathBuf::new(),
                    started_at,
                );
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
    state.claim_running_on_target_for_source(
        &source_id,
        issue.clone(),
        attempt,
        target.clone(),
        workspace_path,
        started_at,
    );
    info!(
        source_id = %source_id,
        issue_id = %issue.id,
        issue_identifier = %issue.identifier,
        execution_target = ?target,
        "worker_dispatched"
    );
    let generation = workers.allocate_generation();
    let writer = config.completion.direct_commit.enabled.then(|| {
        let writer: Arc<dyn TrackerWriter> = tracker.clone();
        writer
    });
    let github_graphql = GitHubGraphqlExecutor::from_tracker_config(&config.tracker).ok();
    let codex = Arc::new(CodexAppServerClient::with_github_graphql(
        config.codex.clone(),
        github_graphql,
    ));
    let runner = SymphonyAgentRunner::new(config, workspace, tracker, writer, codex);
    let worker_issue_key = issue_key.clone();
    let worker_target = target.clone();
    let handle = tokio::spawn(async move {
        let event_issue_key = worker_issue_key.clone();
        let outcome_issue_key = worker_issue_key;
        let callback_tx = channels.event_tx.clone();
        let raw_issue_id = issue.id.clone();
        let mut outcome = runner
            .run_on_target(
                issue,
                attempt,
                worker_target,
                Box::new(move |mut event| {
                    event.issue_id = event_issue_key.clone();
                    let _ = callback_tx.send(WorkerEvent {
                        issue_key: event_issue_key.clone(),
                        generation,
                        event,
                    });
                }),
            )
            .await
            .unwrap_or_else(|error| WorkerOutcome {
                issue_id: raw_issue_id,
                reason: WorkerExitReason::Failed(error.to_string()),
                terminal_state: None,
            });
        outcome.issue_id = outcome_issue_key.clone();
        let _ = channels.outcome_tx.send(WorkerResult {
            issue_key: outcome_issue_key,
            generation,
            outcome,
        });
    });
    workers.insert(issue_key, generation, target, handle);
}

fn apply_worker_event(state: &mut OrchestratorState, workers: &WorkerRegistry, event: WorkerEvent) {
    if workers.matches(&event.issue_key, event.generation) {
        state.apply_codex_event(event.event);
    } else {
        warn!(issue_key = %event.issue_key, generation = event.generation, "stale_worker_event_ignored");
    }
}

async fn apply_worker_outcome(
    state: &mut OrchestratorState,
    configs: &[EffectiveConfig],
    workers: &mut WorkerRegistry,
    outcome: WorkerResult,
) {
    let config = state
        .running
        .get(&outcome.issue_key)
        .and_then(|entry| {
            configs
                .iter()
                .find(|config| config.source.id == entry.source_id)
        })
        .cloned();
    let terminal_issue = outcome
        .outcome
        .terminal_state
        .as_ref()
        .and_then(|terminal_state| {
            state.running.get(&outcome.issue_key).map(|entry| {
                let mut issue = entry.issue.clone();
                issue.state.clone_from(terminal_state);
                (
                    entry.source_id.clone(),
                    entry.execution_target.clone(),
                    issue,
                )
            })
        });
    if !await_worker(workers, &outcome.issue_key, outcome.generation).await {
        warn!(issue_key = %outcome.issue_key, generation = outcome.generation, "stale_worker_outcome_ignored");
        return;
    }
    let Some(config) = config else {
        warn!(issue_key = %outcome.issue_key, "worker_outcome_without_running_entry");
        return;
    };
    if let Some((source_id, target, issue)) = terminal_issue {
        state.release_for_source(&source_id, &issue.id);
        match WorkspaceManager::new(&config.workspace, config.hooks.clone()) {
            Ok(workspace) => {
                if let Err(error) = workspace
                    .remove_for_target(&target, &source_id, &issue)
                    .await
                {
                    warn!(source_id = %source_id, issue_id = %issue.id, issue_identifier = %issue.identifier, execution_target = ?target, error = %error, "terminal_cleanup_failed");
                }
            }
            Err(error) => {
                warn!(source_id = %source_id, issue_id = %issue.id, issue_identifier = %issue.identifier, execution_target = ?target, error = %error, "terminal_cleanup_workspace_unavailable");
            }
        }
        return;
    }
    state.worker_exit_by_key(
        &outcome.issue_key,
        outcome.outcome.reason,
        &config,
        system_monotonic_ms(),
        now_utc(),
    );
}

async fn abort_worker(workers: &mut WorkerRegistry, issue_key: &str) {
    let Some(task) = workers.tasks.remove(issue_key) else {
        return;
    };
    task.handle.abort();
    let _ = task.handle.await;
}

async fn await_worker(workers: &mut WorkerRegistry, issue_key: &str, generation: u64) -> bool {
    if !workers.matches(issue_key, generation) {
        return false;
    }
    let task = workers
        .tasks
        .remove(issue_key)
        .expect("current worker task disappeared");
    let _ = task.handle.await;
    true
}

async fn shutdown_workers(workers: &mut WorkerRegistry) {
    let tasks = std::mem::take(&mut workers.tasks);
    for task in tasks.values() {
        task.handle.abort();
    }
    for (_, task) in tasks {
        let _ = task.handle.await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use serde_yaml::{Mapping, Value};
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::{
        WorkerEvent, WorkerRegistry, WorkerResult, abort_worker, apply_worker_event,
        apply_worker_outcome, await_worker, next_retry_delay, select_execution_target,
        shutdown_workers, worker_dispatch_permitted,
    };
    use crate::agent::runner::WorkerOutcome;
    use crate::config::EffectiveConfig;
    use crate::domain::{CodexEvent, ExecutionTarget, Issue, WorkerExitReason, WorkflowDefinition};
    use crate::orchestrator::OrchestratorState;
    use crate::orchestrator::state::source_workspace_key;
    use crate::time::{now_utc, system_monotonic_ms};

    struct TerminationProbe(Arc<AtomicBool>);

    fn config(workspace_root: &Path) -> EffectiveConfig {
        let mut workspace = Mapping::new();
        workspace.insert(
            Value::String("root".to_string()),
            Value::String(workspace_root.display().to_string()),
        );
        let mut raw_config = Mapping::new();
        raw_config.insert(
            Value::String("workspace".to_string()),
            Value::Mapping(workspace),
        );
        EffectiveConfig::from_workflow(WorkflowDefinition {
            config: raw_config,
            prompt_template: String::new(),
            path: workspace_root.join("WORKFLOW.md"),
        })
        .unwrap()
    }
    impl Drop for TerminationProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: format!("S-{id}"),
            title: "Lifecycle test".to_string(),
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

    fn pending_worker(probe: Arc<AtomicBool>) -> (JoinHandle<()>, oneshot::Receiver<()>) {
        let (started_tx, started_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _probe = TerminationProbe(probe);
            let _ = started_tx.send(());
            pending::<()>().await;
        });
        (handle, started_rx)
    }

    fn event(issue_key: &str) -> CodexEvent {
        CodexEvent {
            issue_id: issue_key.to_string(),
            event: "turn_started".to_string(),
            timestamp: now_utc(),
            session_id: Some("session".to_string()),
            thread_id: Some("thread".to_string()),
            turn_id: Some("turn".to_string()),
            codex_app_server_pid: None,
            message: None,
            absolute_token_totals: None,
            rate_limits: Some(serde_json::json!({"limit": 1})),
        }
    }

    #[tokio::test]
    async fn worker_termination_completes_before_state_release() {
        let mut state = OrchestratorState::default();
        let issue = issue("one");
        state.claim_running(issue.clone(), None, now_utc());
        let probe = Arc::new(AtomicBool::new(false));
        let mut workers = WorkerRegistry::default();
        let (handle, started) = pending_worker(probe.clone());
        workers.insert(issue.id.clone(), 1, ExecutionTarget::Local, handle);
        started.await.unwrap();

        abort_worker(&mut workers, &issue.id).await;
        assert!(probe.load(Ordering::Acquire));
        state.release(&issue.id);
        assert!(!state.running.contains_key(&issue.id));
    }

    #[tokio::test]
    async fn stale_generation_events_and_outcomes_leave_successor_owned() {
        let mut state = OrchestratorState::default();
        let issue = issue("one");
        state.claim_running(issue.clone(), None, now_utc());
        let probe = Arc::new(AtomicBool::new(false));
        let mut workers = WorkerRegistry::default();
        let (handle, started) = pending_worker(probe.clone());
        workers.insert(issue.id.clone(), 2, ExecutionTarget::Local, handle);
        started.await.unwrap();

        apply_worker_event(
            &mut state,
            &workers,
            WorkerEvent {
                issue_key: issue.id.clone(),
                generation: 1,
                event: event(&issue.id),
            },
        );
        assert!(state.codex_rate_limits.is_none());
        assert!(!await_worker(&mut workers, &issue.id, 1).await);
        assert!(workers.matches(&issue.id, 2));
        assert!(state.running.contains_key(&issue.id));

        abort_worker(&mut workers, &issue.id).await;
        assert!(probe.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn retry_dispatch_is_blocked_until_current_worker_terminates() {
        let probe = Arc::new(AtomicBool::new(false));
        let mut workers = WorkerRegistry::default();
        let (handle, started) = pending_worker(probe.clone());
        workers.insert("issue".to_string(), 1, ExecutionTarget::Local, handle);
        started.await.unwrap();
        assert!(!worker_dispatch_permitted(&workers, "issue"));
        abort_worker(&mut workers, "issue").await;
        assert!(probe.load(Ordering::Acquire));
        assert!(worker_dispatch_permitted(&workers, "issue"));
    }

    #[tokio::test]
    async fn shutdown_aborts_and_awaits_every_owned_worker() {
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let mut workers = WorkerRegistry::default();
        let (first_handle, first_started) = pending_worker(first.clone());
        let (second_handle, second_started) = pending_worker(second.clone());
        workers.insert("one".to_string(), 1, ExecutionTarget::Local, first_handle);
        workers.insert("two".to_string(), 2, ExecutionTarget::Local, second_handle);
        first_started.await.unwrap();
        second_started.await.unwrap();

        shutdown_workers(&mut workers).await;

        assert!(workers.tasks.is_empty());
        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn ssh_host_pool_is_deterministic_and_never_falls_back_to_local_when_saturated() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = config(temporary.path());
        config.worker.ssh_hosts = vec!["ssh-a".to_string(), "ssh-b".to_string()];
        config.worker.max_concurrent_agents_per_host = 1;
        let state = OrchestratorState::default();
        let mut workers = WorkerRegistry::default();

        assert_eq!(
            select_execution_target(&workers, &config),
            Some(ExecutionTarget::Ssh {
                host: "ssh-a".to_string()
            })
        );
        workers.insert(
            "one".to_string(),
            1,
            ExecutionTarget::Ssh {
                host: "ssh-a".to_string(),
            },
            tokio::spawn(std::future::pending()),
        );
        assert_eq!(
            select_execution_target(&workers, &config),
            Some(ExecutionTarget::Ssh {
                host: "ssh-b".to_string()
            })
        );
        workers.insert(
            "two".to_string(),
            2,
            ExecutionTarget::Ssh {
                host: "ssh-b".to_string(),
            },
            tokio::spawn(std::future::pending()),
        );
        assert_eq!(select_execution_target(&workers, &config), None);
        assert!(state.running.is_empty());

        shutdown_workers(&mut workers).await;
    }
    #[tokio::test]
    async fn terminal_outcome_releases_claim_and_removes_workspace() {
        let temporary = tempfile::tempdir().unwrap();
        let config = config(temporary.path());
        let mut state = OrchestratorState::default();
        let issue = issue("terminal");
        let workspace = temporary
            .path()
            .join(source_workspace_key("default", &issue.identifier));
        std::fs::create_dir_all(&workspace).unwrap();
        state.claim_running(issue.clone(), None, now_utc());
        let mut workers = WorkerRegistry::default();
        workers.insert(
            issue.id.clone(),
            1,
            ExecutionTarget::Local,
            tokio::spawn(async {}),
        );

        apply_worker_outcome(
            &mut state,
            &[config],
            &mut workers,
            WorkerResult {
                issue_key: issue.id.clone(),
                generation: 1,
                outcome: WorkerOutcome {
                    issue_id: issue.id.clone(),
                    reason: WorkerExitReason::Normal,
                    terminal_state: Some("Done".to_string()),
                },
            },
        )
        .await;

        assert!(!state.running.contains_key(&issue.id));
        assert!(!state.retry_attempts.contains_key(&issue.id));
        assert!(!state.claimed.contains(&issue.id));
        assert!(!workspace.exists());
    }

    #[tokio::test]
    async fn nonterminal_normal_outcome_keeps_continuation_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let config = config(temporary.path());
        let mut state = OrchestratorState::default();
        let issue = issue("continuing");
        state.claim_running(issue.clone(), None, now_utc());
        let mut workers = WorkerRegistry::default();
        workers.insert(
            issue.id.clone(),
            1,
            ExecutionTarget::Local,
            tokio::spawn(async {}),
        );

        apply_worker_outcome(
            &mut state,
            &[config],
            &mut workers,
            WorkerResult {
                issue_key: issue.id.clone(),
                generation: 1,
                outcome: WorkerOutcome {
                    issue_id: issue.id.clone(),
                    reason: WorkerExitReason::Normal,
                    terminal_state: None,
                },
            },
        )
        .await;

        assert!(state.running.is_empty());
        assert!(state.retry_attempts.contains_key(&issue.id));
        assert!(state.claimed.contains(&issue.id));
    }

    #[test]
    fn retry_timer_wakes_before_the_independent_poll_deadline() {
        let mut state = OrchestratorState::default();
        let issue = issue("one");
        state.schedule_retry_now(&issue, 1, None);
        let due_at_ms = system_monotonic_ms().saturating_add(1_000);
        state.retry_attempts.get_mut(&issue.id).unwrap().due_at_ms = due_at_ms;

        assert_eq!(state.next_retry_due_at_ms(), Some(due_at_ms));
        assert!(next_retry_delay(&state) < Duration::from_secs(5));
    }
}
