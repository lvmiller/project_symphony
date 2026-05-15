use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use serde_json::json;
use symphony::config::{
    AgentConfig, CodexConfig, CompletionConfig, EffectiveConfig, GithubConfig,
    GithubProjectOwnerType, HooksConfig, PollingConfig, TrackerConfig, WorkspaceConfig,
};
use symphony::domain::{BlockerRef, CodexEvent, Issue, TokenTotals, WorkerExitReason};
use symphony::orchestrator::retry::{
    continuation_retry_due_at_ms, failure_retry_delay_ms, retry_is_due,
};
use symphony::orchestrator::scheduler::{
    DispatchIneligibleReason, dispatch_ineligible_reason, is_dispatch_eligible, sort_for_dispatch,
};
use symphony::orchestrator::state::{OrchestratorState, ReconcileDecision};

fn ts(ms: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap()
}

fn config(
    max_global: usize,
    per_state: impl IntoIterator<Item = (&'static str, usize)>,
) -> EffectiveConfig {
    EffectiveConfig {
        workflow_path: PathBuf::from("workflow.yml"),
        workflow_dir: PathBuf::from("."),
        prompt_template: String::new(),
        tracker: TrackerConfig {
            kind: "github".to_string(),
            endpoint: "https://api.github.com/graphql".to_string(),
            api_key: Some("token".to_string()),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            terminal_states: vec![
                "Done".to_string(),
                "Closed".to_string(),
                "Canceled".to_string(),
            ],
            github: Some(GithubConfig {
                repository_owner: "owner".to_string(),
                repository_name: "repo".to_string(),
                project_owner_type: GithubProjectOwnerType::Organization,
                project_owner_login: "org".to_string(),
                project_number: 1,
                status_field_name: "Status".to_string(),
                priority_field_name: Some("Priority".to_string()),
                blocker_field_name: Some("Blocked by".to_string()),
                blocker_label_prefix: None,
                priority_labels: BTreeMap::new(),
            }),
        },
        polling: PollingConfig { interval_ms: 1_000 },
        workspace: WorkspaceConfig {
            root: PathBuf::from("work"),
        },
        hooks: HooksConfig::default(),
        agent: AgentConfig {
            max_concurrent_agents: max_global,
            max_turns: 20,
            max_retry_backoff_ms: 30_000,
            max_concurrent_agents_by_state: per_state
                .into_iter()
                .map(|(state, limit)| (state.to_string(), limit))
                .collect(),
        },
        codex: CodexConfig {
            command: "codex".to_string(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: 60_000,
            read_timeout_ms: 10_000,
            stall_timeout_ms: 0,
        },
        completion: CompletionConfig::default(),
    }
}

fn issue(id: &str, identifier: &str, state: &str) -> Issue {
    Issue {
        id: id.to_string(),
        identifier: identifier.to_string(),
        title: format!("Issue {identifier}"),
        description: None,
        priority: None,
        state: state.to_string(),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[test]
fn dispatch_sort_orders_priority_created_and_identifier_with_missing_created_last() {
    let mut issues = vec![
        Issue {
            priority: Some(2),
            created_at: Some(ts(1_000)),
            ..issue("4", "S-004", "Todo")
        },
        Issue {
            priority: None,
            created_at: Some(ts(0)),
            ..issue("5", "S-005", "Todo")
        },
        Issue {
            priority: Some(1),
            created_at: None,
            ..issue("3", "S-003", "Todo")
        },
        Issue {
            priority: Some(1),
            created_at: Some(ts(2_000)),
            ..issue("2", "S-002", "Todo")
        },
        Issue {
            priority: Some(1),
            created_at: Some(ts(2_000)),
            ..issue("1", "S-001", "Todo")
        },
    ];

    sort_for_dispatch(&mut issues);

    let ordered: Vec<_> = issues
        .iter()
        .map(|issue| issue.identifier.as_str())
        .collect();
    assert_eq!(ordered, vec!["S-001", "S-002", "S-003", "S-004", "S-005"]);
}

#[test]
fn todo_issues_wait_for_all_blockers_to_be_terminal() {
    let config = config(2, []);
    let state = OrchestratorState::default();
    let mut blocked = issue("todo", "S-001", "Todo");
    blocked.blocked_by = vec![
        BlockerRef {
            id: Some("done".to_string()),
            identifier: None,
            state: Some("Done".to_string()),
        },
        BlockerRef {
            id: Some("active".to_string()),
            identifier: None,
            state: Some("In Progress".to_string()),
        },
    ];
    assert_eq!(
        dispatch_ineligible_reason(&blocked, &state, &config),
        Some(DispatchIneligibleReason::TodoBlocked)
    );

    blocked.blocked_by[1].state = Some("Closed".to_string());
    assert!(is_dispatch_eligible(&blocked, &state, &config));

    blocked.blocked_by[1].state = None;
    assert_eq!(
        dispatch_ineligible_reason(&blocked, &state, &config),
        Some(DispatchIneligibleReason::TodoBlocked)
    );
}

#[test]
fn concurrency_uses_global_and_per_state_slots_and_release_frees_claims() {
    let config = config(2, [("todo", 1)]);
    let mut state = OrchestratorState::default();
    let todo1 = issue("todo-1", "S-001", "Todo");
    let todo2 = issue("todo-2", "S-002", "Todo");
    let progress = issue("prog-1", "S-003", "In Progress");

    state.claim_running(todo1.clone(), None, ts(0));
    assert_eq!(
        dispatch_ineligible_reason(&todo2, &state, &config),
        Some(DispatchIneligibleReason::StateSlotsExhausted)
    );
    assert!(is_dispatch_eligible(&progress, &state, &config));

    state.claim_running(progress.clone(), None, ts(0));
    assert_eq!(
        dispatch_ineligible_reason(&todo2, &state, &config),
        Some(DispatchIneligibleReason::GlobalSlotsExhausted)
    );

    state.release(&todo1.id);
    assert_eq!(
        dispatch_ineligible_reason(&progress, &state, &config),
        Some(DispatchIneligibleReason::AlreadyRunningOrClaimed)
    );
    state.release(&progress.id);
    assert!(is_dispatch_eligible(&todo2, &state, &config));
}

#[test]
fn dispatch_rejects_different_issue_ids_with_same_workspace_key() {
    let config = config(2, []);
    let mut state = OrchestratorState::default();
    let running = issue("id-1", "S/001", "Todo");
    let colliding = issue("id-2", "S?001", "Todo");

    state.claim_running(running.clone(), None, ts(0));

    assert_eq!(
        dispatch_ineligible_reason(&colliding, &state, &config),
        Some(DispatchIneligibleReason::AlreadyRunningOrClaimed)
    );

    state.release(&running.id);
    assert!(is_dispatch_eligible(&colliding, &state, &config));
}

#[test]
fn retry_claims_block_workspace_key_until_released() {
    let config = config(2, []);
    let mut state = OrchestratorState::default();
    let running = issue("id-1", "S/001", "In Progress");
    let colliding = issue("id-2", "S?001", "In Progress");
    state.claim_running(running.clone(), None, ts(0));

    let retry = state
        .worker_exit(
            &running.id,
            WorkerExitReason::Normal,
            &config,
            50,
            ts(1_000),
        )
        .unwrap();

    assert_eq!(retry.workspace_key, "S_001");
    assert_eq!(
        dispatch_ineligible_reason(&colliding, &state, &config),
        Some(DispatchIneligibleReason::AlreadyRunningOrClaimed)
    );

    state.release(&running.id);
    assert!(is_dispatch_eligible(&colliding, &state, &config));
}

#[test]
fn worker_exit_schedules_normal_continuation_retry_and_keeps_retry_claim() {
    let config = config(1, []);
    let mut state = OrchestratorState::default();
    let issue = issue("id", "S-001", "In Progress");
    state.claim_running(issue.clone(), None, ts(0));

    let retry = state
        .worker_exit(&issue.id, WorkerExitReason::Normal, &config, 50, ts(2_500))
        .unwrap();

    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.due_at_ms, continuation_retry_due_at_ms(50));
    assert_eq!(retry.error, None);
    assert!(state.completed.contains(&issue.id));
    assert!(state.claimed.contains(&issue.id));
    assert!(retry_is_due(retry.due_at_ms, 1_050));
    assert!(!retry_is_due(retry.due_at_ms, 1_049));
}

#[test]
fn worker_exit_schedules_abnormal_exponential_retry_capped() {
    let config = config(1, []);
    let mut state = OrchestratorState::default();
    let issue = issue("id", "S-001", "In Progress");
    state.claim_running(issue.clone(), Some(2), ts(0));

    let retry = state
        .worker_exit(
            &issue.id,
            WorkerExitReason::Failed("boom".to_string()),
            &config,
            100,
            ts(10_000),
        )
        .unwrap();

    assert_eq!(retry.attempt, 3);
    assert_eq!(
        failure_retry_delay_ms(retry.attempt, config.agent.max_retry_backoff_ms),
        30_000
    );
    assert_eq!(retry.due_at_ms, 30_100);
    assert_eq!(retry.error.as_deref(), Some("boom"));
    assert!(state.claimed.contains(&issue.id));
    assert_eq!(state.ended_runtime_seconds, 10.0);
}

#[test]
fn no_running_reconcile_is_noop_and_does_not_create_entries() {
    let config = config(1, []);
    let mut state = OrchestratorState::default();

    let decision =
        state.reconcile_running_issue("missing", Some(&issue("id", "S-001", "Todo")), &config);

    assert_eq!(decision, ReconcileDecision::NoRunningEntry);
    assert!(state.running.is_empty());
}

#[test]
fn reconcile_refreshes_active_state_and_marks_terminal_or_non_active_for_cancel() {
    let config = config(1, []);
    let mut state = OrchestratorState::default();
    let active = issue("id", "S-001", "Todo");
    state.claim_running(active, None, ts(0));

    let refreshed = Issue {
        state: "In Progress".to_string(),
        title: "updated".to_string(),
        ..issue("id", "S-001", "Todo")
    };
    assert_eq!(
        state.reconcile_running_issue("id", Some(&refreshed), &config),
        ReconcileDecision::RefreshedActive
    );
    let entry = state.running.get("id").unwrap();
    assert_eq!(entry.issue.title, "updated");
    assert_eq!(entry.issue.state, "In Progress");
    assert!(!entry.cancel_requested);

    let terminal = Issue {
        state: "Done".to_string(),
        ..issue("id", "S-001", "Todo")
    };
    assert_eq!(
        state.reconcile_running_issue("id", Some(&terminal), &config),
        ReconcileDecision::CancelTerminal
    );
    assert!(state.running.get("id").unwrap().cancel_requested);

    state.running.get_mut("id").unwrap().cancel_requested = false;
    let inactive = Issue {
        state: "Backlog".to_string(),
        ..issue("id", "S-001", "Todo")
    };
    assert_eq!(
        state.reconcile_running_issue("id", Some(&inactive), &config),
        ReconcileDecision::CancelNonActive
    );
    assert!(state.running.get("id").unwrap().cancel_requested);

    state.running.get_mut("id").unwrap().cancel_requested = false;
    assert_eq!(
        state.reconcile_running_issue("id", None, &config),
        ReconcileDecision::MissingFromTracker
    );
    assert!(state.running.get("id").unwrap().cancel_requested);
}

#[test]
fn stall_detection_is_disabled_by_non_positive_timeout_and_uses_last_event_when_enabled() {
    let mut disabled = config(2, []);
    disabled.codex.stall_timeout_ms = 0;
    let mut enabled = disabled.clone();
    enabled.codex.stall_timeout_ms = 1_000;
    let mut state = OrchestratorState::default();
    let stale = issue("stale", "S-001", "In Progress");
    let fresh = issue("fresh", "S-002", "In Progress");
    state.claim_running(stale.clone(), None, ts(0));
    state.claim_running(fresh.clone(), None, ts(0));
    state.apply_codex_event(CodexEvent {
        issue_id: fresh.id.clone(),
        event: "turn_started".to_string(),
        timestamp: ts(1_500),
        session_id: Some("session".to_string()),
        thread_id: Some("thread".to_string()),
        turn_id: Some("turn".to_string()),
        codex_app_server_pid: None,
        message: None,
        absolute_token_totals: None,
        rate_limits: None,
    });

    assert!(state.stalled_issue_ids(&disabled, ts(2_000)).is_empty());
    assert_eq!(
        state.stalled_issue_ids(&enabled, ts(2_000)),
        vec!["stale".to_string()]
    );
}

#[test]
fn codex_events_aggregate_only_positive_token_deltas_and_latest_rate_limits() {
    let mut state = OrchestratorState::default();
    let issue = issue("id", "S-001", "In Progress");
    state.claim_running(issue.clone(), None, ts(0));

    state.apply_codex_event(CodexEvent {
        issue_id: issue.id.clone(),
        event: "turn_started".to_string(),
        timestamp: ts(100),
        session_id: Some("session".to_string()),
        thread_id: Some("thread".to_string()),
        turn_id: Some("turn".to_string()),
        codex_app_server_pid: Some(42),
        message: Some("started".to_string()),
        absolute_token_totals: Some(TokenTotals {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        }),
        rate_limits: Some(json!({"remaining": 10})),
    });
    state.apply_codex_event(CodexEvent {
        issue_id: issue.id.clone(),
        event: "message".to_string(),
        timestamp: ts(200),
        session_id: Some("session".to_string()),
        thread_id: Some("thread".to_string()),
        turn_id: Some("turn".to_string()),
        codex_app_server_pid: None,
        message: None,
        absolute_token_totals: Some(TokenTotals {
            input_tokens: 8,
            output_tokens: 9,
            total_tokens: 17,
        }),
        rate_limits: Some(json!({"remaining": 8})),
    });

    assert_eq!(
        state.codex_totals,
        TokenTotals {
            input_tokens: 10,
            output_tokens: 9,
            total_tokens: 17
        }
    );
    assert_eq!(state.codex_rate_limits, Some(json!({"remaining": 8})));
    let session = state
        .running
        .get(&issue.id)
        .unwrap()
        .live_session
        .as_ref()
        .unwrap();
    assert_eq!(session.turn_count, 1);
    assert_eq!(session.codex_app_server_pid, Some(42));
}

#[test]
fn snapshot_contains_running_retry_token_rate_limit_and_elapsed_fields() {
    let config = config(1, []);
    let mut state = OrchestratorState::default();
    let running_issue = issue("id", "S-001", "In Progress");
    state.claim_running(running_issue.clone(), None, ts(1_000));
    state.apply_codex_event(CodexEvent {
        issue_id: running_issue.id.clone(),
        event: "turn_started".to_string(),
        timestamp: ts(1_100),
        session_id: Some("session".to_string()),
        thread_id: Some("thread".to_string()),
        turn_id: Some("turn".to_string()),
        codex_app_server_pid: None,
        message: None,
        absolute_token_totals: Some(TokenTotals {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        }),
        rate_limits: Some(json!({"reset": 123})),
    });
    let other = issue("other", "S-002", "In Progress");
    state.claim_running(other.clone(), None, ts(0));
    state
        .worker_exit(
            &other.id,
            WorkerExitReason::TimedOut("timeout".to_string()),
            &config,
            2_000,
            ts(2_000),
        )
        .unwrap();

    let snapshot = state.snapshot(ts(3_500));

    assert_eq!(snapshot.running.len(), 1);
    assert_eq!(snapshot.running[0].issue_id, "id");
    assert_eq!(snapshot.running[0].issue_identifier, "S-001");
    assert_eq!(snapshot.running[0].state, "In Progress");
    assert_eq!(snapshot.running[0].session_id.as_deref(), Some("session"));
    assert_eq!(snapshot.running[0].turn_count, 1);
    assert_eq!(snapshot.retrying.len(), 1);
    assert_eq!(snapshot.retrying[0].issue_id, "other");
    assert_eq!(
        snapshot.codex_totals,
        TokenTotals {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3
        }
    );
    assert_eq!(snapshot.rate_limits, Some(json!({"reset": 123})));
    assert_eq!(snapshot.seconds_running, 4.5);
}
