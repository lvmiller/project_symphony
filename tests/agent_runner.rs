use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use symphony::agent::codex::{CodexClient, TurnOutcome};
use symphony::agent::runner::{AgentRunner, SymphonyAgentRunner};
use symphony::config::{
    AgentConfig, CodexConfig, EffectiveConfig, GithubConfig, GithubProjectOwnerType, HooksConfig,
    PollingConfig, TrackerConfig, WorkspaceConfig,
};
use symphony::domain::{CodexEvent, Issue, WorkerExitReason};
use symphony::error::{Result, SymphonyError};
use symphony::tracker::TrackerClient;
use symphony::workspace::WorkspaceManager;
use tempfile::TempDir;

#[tokio::test]
async fn runner_uses_initial_prompt_then_continuation_and_refreshes_after_successful_turns() {
    let temp = TempDir::new().unwrap();
    let issue = issue("ISS-1", "active");
    let config = config(
        temp.path(),
        2,
        "Original task: {{ issue.title }} -- {{ issue.description }}",
    );
    let workspace = WorkspaceManager::new(&config.workspace, HooksConfig::default()).unwrap();
    let codex = Arc::new(FakeCodex::default());
    let tracker = Arc::new(FakeTracker::new(vec![
        issue_with_state("ISS-1", "active"),
        issue_with_state("ISS-1", "active"),
    ]));
    let runner = SymphonyAgentRunner::new(config, workspace, tracker.clone(), codex.clone());

    let outcome = runner.run(issue, Some(3), Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.issue_id, "ISS-1");
    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    let prompts = codex.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("Original task: Fix allocator"));
    assert!(prompts[0].contains("unique original issue body"));
    assert!(prompts[1].contains("Continue working on the same issue"));
    assert!(prompts[1].contains("attempt=3"));
    assert!(!prompts[1].contains("unique original issue body"));
    assert_eq!(
        tracker.requested_ids(),
        vec![vec!["ISS-1".to_string()], vec!["ISS-1".to_string()]]
    );
}

#[tokio::test]
async fn runner_runs_lifecycle_hooks_and_stops_on_terminal_state() {
    let temp = TempDir::new().unwrap();
    let events = temp.path().join("events");
    let mut hooks = HooksConfig {
        timeout_ms: 60_000,
        ..HooksConfig::default()
    };
    hooks.before_run = Some(format!("printf before >> {}", shell_quote(&events)));
    hooks.after_run = Some(format!("printf after >> {}; exit 9", shell_quote(&events)));

    let issue = issue("ISS-2", "active");
    let config = config(temp.path(), 3, "{{ issue.identifier }}");
    let workspace = WorkspaceManager::new(&config.workspace, hooks).unwrap();
    let codex = Arc::new(FakeCodex::default());
    let tracker = Arc::new(FakeTracker::new(vec![issue_with_state("ISS-2", "done")]));
    let runner = SymphonyAgentRunner::new(config, workspace, tracker, codex.clone());

    let outcome = runner.run(issue, None, Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert_eq!(codex.prompts().len(), 1);
    assert_eq!(std::fs::read_to_string(events).unwrap(), "beforeafter");
}

#[tokio::test]
async fn runner_converts_codex_failure_to_worker_outcome_and_runs_after_run() {
    let temp = TempDir::new().unwrap();
    let events = temp.path().join("events");
    let mut hooks = HooksConfig {
        timeout_ms: 60_000,
        ..HooksConfig::default()
    };
    hooks.after_run = Some(format!("printf after >> {}", shell_quote(&events)));

    let issue = issue("ISS-3", "active");
    let config = config(temp.path(), 1, "{{ issue.identifier }}");
    let workspace = WorkspaceManager::new(&config.workspace, hooks).unwrap();
    let codex = Arc::new(FakeCodex::failing("boom"));
    let tracker = Arc::new(FakeTracker::new(Vec::new()));
    let runner = SymphonyAgentRunner::new(config, workspace, tracker, codex);

    let outcome = runner.run(issue, None, Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.issue_id, "ISS-3");
    match outcome.reason {
        WorkerExitReason::Failed(message) => assert!(message.contains("boom")),
        other => panic!("expected failure, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(events).unwrap(), "after");
}

#[derive(Default)]
struct FakeCodex {
    prompts: Mutex<Vec<String>>,
    failure: Option<&'static str>,
}

impl FakeCodex {
    fn failing(message: &'static str) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            failure: Some(message),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl CodexClient for FakeCodex {
    async fn run_turn(
        &self,
        _workspace: &Path,
        prompt: &str,
        _on_event: &mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<TurnOutcome> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        if let Some(message) = self.failure {
            return Err(SymphonyError::codex("fake", message));
        }
        Ok(TurnOutcome {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
        })
    }
}

struct FakeTracker {
    states: Mutex<Vec<Issue>>,
    requested_ids: Mutex<Vec<Vec<String>>>,
}

impl FakeTracker {
    fn new(states: Vec<Issue>) -> Self {
        Self {
            states: Mutex::new(states.into_iter().rev().collect()),
            requested_ids: Mutex::new(Vec::new()),
        }
    }

    fn requested_ids(&self) -> Vec<Vec<String>> {
        self.requested_ids.lock().unwrap().clone()
    }
}

#[async_trait]
impl TrackerClient for FakeTracker {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>> {
        Ok(Vec::new())
    }

    async fn fetch_issues_by_states(&self, _state_names: &[String]) -> Result<Vec<Issue>> {
        Ok(Vec::new())
    }

    async fn fetch_issue_states_by_ids(&self, issue_ids: &[String]) -> Result<Vec<Issue>> {
        self.requested_ids.lock().unwrap().push(issue_ids.to_vec());
        Ok(self.states.lock().unwrap().pop().into_iter().collect())
    }
}

fn issue(id: &str, state: &str) -> Issue {
    Issue {
        id: id.to_string(),
        identifier: id.to_string(),
        title: "Fix allocator".to_string(),
        description: Some("unique original issue body".to_string()),
        priority: None,
        state: state.to_string(),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    }
}

fn issue_with_state(id: &str, state: &str) -> Issue {
    let mut issue = issue(id, state);
    issue.state = state.to_string();
    issue
}

fn config(root: &Path, max_turns: u32, prompt_template: &str) -> EffectiveConfig {
    EffectiveConfig {
        workflow_path: root.join("WORKFLOW.md"),
        workflow_dir: root.to_path_buf(),
        prompt_template: prompt_template.to_string(),
        tracker: TrackerConfig {
            kind: "github".to_string(),
            endpoint: "https://api.github.com/graphql".to_string(),
            api_key: Some("redacted".to_string()),
            active_states: vec!["active".to_string()],
            terminal_states: vec!["done".to_string()],
            github: Some(GithubConfig {
                repository_owner: "owner".to_string(),
                repository_name: "repo".to_string(),
                project_owner_type: GithubProjectOwnerType::Organization,
                project_owner_login: "owner".to_string(),
                project_number: 1,
                status_field_name: "Status".to_string(),
                priority_field_name: None,
                blocker_field_name: None,
                blocker_label_prefix: None,
                priority_labels: BTreeMap::new(),
            }),
        },
        polling: PollingConfig {
            interval_ms: 30_000,
        },
        workspace: WorkspaceConfig {
            root: root.join("workspaces"),
        },
        hooks: HooksConfig::default(),
        agent: AgentConfig {
            max_concurrent_agents: 1,
            max_turns,
            max_retry_backoff_ms: 1_000,
            max_concurrent_agents_by_state: BTreeMap::new(),
        },
        codex: CodexConfig {
            command: "codex app-server".to_string(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: 1_000,
            read_timeout_ms: 1_000,
            stall_timeout_ms: 1_000,
        },
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
