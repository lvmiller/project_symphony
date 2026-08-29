use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use symphony::agent::codex::{CodexClient, CodexSession, TurnOutcome};
use symphony::agent::runner::{AgentRunner, SymphonyAgentRunner};
use symphony::config::{
    AgentConfig, CodexConfig, CompletionConfig, DirectCommitCompletionConfig, EffectiveConfig,
    GithubConfig, GithubProjectOwnerType, GithubRepositoryConfig, HooksConfig, PollingConfig,
    ServerConfig, SourceConfig, TrackerConfig, WorkerConfig, WorkspaceCleanupAfterSuccess,
    WorkspaceCleanupConfig, WorkspaceConfig,
};
use symphony::domain::{CodexEvent, Issue, WorkerExitReason};
use symphony::error::{Result, SymphonyError};
use symphony::tracker::{TrackerClient, TrackerWriter};
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
    let runner = SymphonyAgentRunner::new(
        config,
        workspace,
        tracker.clone(),
        Some(tracker.clone()),
        codex.clone(),
    );

    let outcome = runner.run(issue, Some(3), Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.issue_id, "ISS-1");
    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert_eq!(outcome.terminal_state, None);
    let prompts = codex.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("Original task: Fix allocator"));
    assert!(prompts[0].contains("unique original issue body"));
    assert!(prompts[1].contains("Continue working on the same issue"));
    assert!(prompts[1].contains("attempt=3"));
    assert!(!prompts[1].contains("unique original issue body"));
    assert_eq!(
        codex.session_counts(),
        (1, 1),
        "all continuation turns share one worker session"
    );
    assert_eq!(
        tracker.requested_ids(),
        vec![vec!["ISS-1".to_string()], vec!["ISS-1".to_string()]]
    );
}

#[tokio::test]
async fn runner_marks_terminal_refresh_on_normal_exit_and_runs_lifecycle_hooks() {
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
    let runner = SymphonyAgentRunner::new(
        config,
        workspace,
        tracker.clone(),
        Some(tracker),
        codex.clone(),
    );

    let outcome = runner.run(issue, None, Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert_eq!(outcome.terminal_state.as_deref(), Some("done"));
    assert_eq!(codex.prompts().len(), 1);
    assert_eq!(std::fs::read_to_string(events).unwrap(), "beforeafter");
}

#[tokio::test]
async fn runner_treats_direct_completion_without_changes_as_normal_skip() {
    let temp = TempDir::new().unwrap();
    let mut issue = issue("ISS-4", "active");
    issue.title = "[Medium] Fix allocator".to_string();
    configure_trusted_issue(&mut issue, 4);

    let mut config = config(temp.path(), 1, "{{ issue.identifier }}");
    config.hooks = HooksConfig {
        after_create: Some("git init".to_string()),
        timeout_ms: 60_000,
        ..HooksConfig::default()
    };
    config.completion = CompletionConfig {
        direct_commit: DirectCommitCompletionConfig {
            enabled: true,
            dry_run: false,
            base_branch: "main".to_string(),
            high_review_state: "In review".to_string(),
            auto_approved_state: "Done".to_string(),
            started_state: None,
            commit_author_name: "Symphony".to_string(),
            commit_author_email: "symphony@users.noreply.github.com".to_string(),
        },
    };
    let workspace = WorkspaceManager::new(&config.workspace, config.hooks.clone()).unwrap();
    let (_, workspace_path) = workspace
        .workspace_path_for_identifier(&issue.identifier)
        .unwrap();
    let codex = Arc::new(FakeCodex::default());
    let tracker = Arc::new(FakeTracker::new(vec![issue.clone(), issue.clone()]));
    let runner = SymphonyAgentRunner::new(
        config,
        workspace,
        tracker.clone(),
        Some(tracker.clone()),
        codex.clone(),
    );

    let outcome = runner.run(issue, None, Box::new(|_| {})).await.unwrap();

    assert_eq!(outcome.issue_id, "ISS-4");
    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert_eq!(codex.prompts().len(), 1);
    assert_eq!(
        tracker.requested_ids(),
        vec![vec!["ISS-4".to_string()], vec!["ISS-4".to_string()]]
    );
    assert!(workspace_path.exists());
}

#[tokio::test]
async fn runner_removes_workspace_after_successful_direct_commit_completion() {
    let temp = TempDir::new().unwrap();
    let server = TestServer::new(vec![
        ok(project_status_lookup(
            GithubProjectOwnerType::Organization,
            "owner",
            1,
            &[("Done", "DONE_OPTION"), ("In review", "REVIEW_OPTION")],
        )),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let mut issue = issue("ISS-5", "active");
    issue.title = "[Medium] Fix allocator".to_string();
    configure_trusted_issue(&mut issue, 5);

    let mut config = config(temp.path(), 1, "{{ issue.identifier }}");
    config.tracker.endpoint = format!("{}/graphql", server.url());
    config.tracker.allow_insecure_loopback = true;
    config.completion = CompletionConfig {
        direct_commit: DirectCommitCompletionConfig {
            enabled: true,
            dry_run: false,
            base_branch: "main".to_string(),
            high_review_state: "In review".to_string(),
            auto_approved_state: "Done".to_string(),
            started_state: None,
            commit_author_name: "Symphony".to_string(),
            commit_author_email: "symphony@users.noreply.github.com".to_string(),
        },
    };
    let workspace = WorkspaceManager::new(&config.workspace, config.hooks.clone()).unwrap();
    let prepared = workspace
        .create_for_identifier(&issue.identifier)
        .await
        .unwrap();
    let remote = temp.path().join("remote.git");
    init_workspace_repo(&prepared.path, &remote, "initial\n");
    let codex = Arc::new(FakeCodex::writing("README.md", "initial\nchanged\n"));
    let tracker = Arc::new(FakeTracker::new(vec![issue.clone(), issue.clone()]));
    let runner = SymphonyAgentRunner::new_with_test_authenticated_remote_url(
        config,
        workspace,
        tracker.clone(),
        Some(tracker),
        codex,
        authenticated_file_url(&remote),
    );

    let outcome = runner
        .run(issue.clone(), None, Box::new(|_| {}))
        .await
        .unwrap();

    assert_eq!(outcome.issue_id, "ISS-5");
    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert!(!prepared.path.exists());
    assert_eq!(
        git_show(&remote, "refs/heads/main:README.md"),
        "initial\nchanged\n"
    );
    assert!(
        server.requests().is_empty(),
        "runner must use TrackerWriter, not a private GraphQL client"
    );
}

#[tokio::test]
async fn runner_keeps_workspace_after_committed_completion_when_cleanup_policy_is_never() {
    let temp = TempDir::new().unwrap();
    let server = TestServer::new(vec![
        ok(project_status_lookup(
            GithubProjectOwnerType::Organization,
            "owner",
            1,
            &[("Done", "DONE_OPTION"), ("In review", "REVIEW_OPTION")],
        )),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let mut issue = issue("ISS-6", "active");
    issue.title = "[Medium] Fix allocator".to_string();
    configure_trusted_issue(&mut issue, 6);

    let mut config = config(temp.path(), 1, "{{ issue.identifier }}");
    config.tracker.endpoint = format!("{}/graphql", server.url());
    config.tracker.allow_insecure_loopback = true;
    config.workspace.cleanup.after_success = WorkspaceCleanupAfterSuccess::Never;
    config.completion = CompletionConfig {
        direct_commit: DirectCommitCompletionConfig {
            enabled: true,
            dry_run: false,
            base_branch: "main".to_string(),
            high_review_state: "In review".to_string(),
            auto_approved_state: "Done".to_string(),
            started_state: None,
            commit_author_name: "Symphony".to_string(),
            commit_author_email: "symphony@users.noreply.github.com".to_string(),
        },
    };
    let workspace = WorkspaceManager::new(&config.workspace, config.hooks.clone()).unwrap();
    let prepared = workspace
        .create_for_identifier(&issue.identifier)
        .await
        .unwrap();
    let remote = temp.path().join("remote-never.git");
    init_workspace_repo(&prepared.path, &remote, "initial\n");
    let codex = Arc::new(FakeCodex::writing("README.md", "initial\nchanged\n"));
    let tracker = Arc::new(FakeTracker::new(vec![issue.clone(), issue.clone()]));
    let runner = SymphonyAgentRunner::new_with_test_authenticated_remote_url(
        config,
        workspace,
        tracker.clone(),
        Some(tracker),
        codex,
        authenticated_file_url(&remote),
    );

    let outcome = runner
        .run(issue.clone(), None, Box::new(|_| {}))
        .await
        .unwrap();

    assert_eq!(outcome.issue_id, "ISS-6");
    assert_eq!(outcome.reason, WorkerExitReason::Normal);
    assert!(prepared.path.exists());
    assert_eq!(
        git_show(&remote, "refs/heads/main:README.md"),
        "initial\nchanged\n"
    );
    assert!(
        server.requests().is_empty(),
        "runner must use TrackerWriter, not a private GraphQL client"
    );
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
    let runner = SymphonyAgentRunner::new(config, workspace, tracker, None, codex);

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
    session_starts: Mutex<u32>,
    session_shutdowns: Mutex<u32>,
    failure: Option<&'static str>,
    write_file: Option<(String, String)>,
}

impl FakeCodex {
    fn failing(message: &'static str) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            session_starts: Mutex::new(0),
            session_shutdowns: Mutex::new(0),
            failure: Some(message),
            write_file: None,
        }
    }

    fn writing(path: impl Into<String>, contents: impl Into<String>) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            session_starts: Mutex::new(0),
            session_shutdowns: Mutex::new(0),
            failure: None,
            write_file: Some((path.into(), contents.into())),
        }
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    fn session_counts(&self) -> (u32, u32) {
        (
            *self.session_starts.lock().unwrap(),
            *self.session_shutdowns.lock().unwrap(),
        )
    }
}

#[async_trait]
impl CodexClient for FakeCodex {
    async fn start_session<'a>(
        &'a self,
        workspace: &Path,
        _on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Box<dyn CodexSession + 'a>> {
        *self.session_starts.lock().unwrap() += 1;
        Ok(Box::new(FakeCodexSession {
            codex: self,
            workspace: workspace.to_path_buf(),
        }))
    }
}

struct FakeCodexSession<'a> {
    codex: &'a FakeCodex,
    workspace: std::path::PathBuf,
}

#[async_trait]
impl CodexSession for FakeCodexSession<'_> {
    async fn run_turn(&mut self, prompt: &str) -> Result<TurnOutcome> {
        self.codex.prompts.lock().unwrap().push(prompt.to_string());
        if let Some(message) = self.codex.failure {
            return Err(SymphonyError::codex("fake", message));
        }
        if let Some((path, contents)) = &self.codex.write_file {
            let file_path = self.workspace.join(path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(file_path, contents).unwrap();
        }
        Ok(TurnOutcome {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            session_id: "thread-turn".to_string(),
        })
    }

    async fn shutdown(&mut self) {
        *self.codex.session_shutdowns.lock().unwrap() += 1;
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

#[async_trait]
impl TrackerWriter for FakeTracker {
    async fn move_issue_to_state(&self, _issue: &Issue, _target_state: &str) -> Result<()> {
        Ok(())
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

fn configure_trusted_issue(issue: &mut Issue, number: u32) {
    issue.identifier = format!("owner/repo#{number}");
    issue.url = Some(format!("https://github.com/owner/repo/issues/{number}"));
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
        source: SourceConfig {
            id: "default".to_string(),
        },
        tracker: TrackerConfig {
            kind: "github".to_string(),
            endpoint: "https://api.github.com/graphql".to_string(),
            api_key: Some("redacted".to_string()),
            allow_insecure_loopback: false,
            active_states: vec!["active".to_string()],
            terminal_states: vec!["done".to_string()],
            github: Some(GithubConfig {
                repository_owner: "owner".to_string(),
                repository_name: "repo".to_string(),
                repositories: vec![GithubRepositoryConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }],
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
            remote_root: format!(
                "/{}",
                root.to_string_lossy().replace('\\', "/").replace(':', "")
            ),
            cleanup: WorkspaceCleanupConfig::default(),
            population: Default::default(),
            retention: Default::default(),
        },
        hooks: HooksConfig::default(),
        worker: WorkerConfig::default(),
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
        completion: CompletionConfig::default(),
        server: ServerConfig::default(),
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn init_workspace_repo(workspace: &Path, remote: &Path, initial_readme: &str) {
    run_git(
        workspace.parent().unwrap(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    run_git(workspace, &["init"]);
    std::fs::write(workspace.join("README.md"), initial_readme).unwrap();
    run_git(workspace, &["add", "README.md"]);
    run_git(
        workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-m",
            "initial",
        ],
    );
    run_git(workspace, &["branch", "-M", "main"]);
    run_git(
        workspace,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(workspace, &["push", "origin", "main"]);
    run_git(
        workspace,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
}

fn authenticated_file_url(remote: &Path) -> String {
    let path = remote.to_string_lossy().replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_show(git_dir: &Path, spec: &str) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["show", "--format=%B", spec])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git show {:?} failed\nstdout={}\nstderr={}",
        spec,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn project_status_lookup(
    owner_type: GithubProjectOwnerType,
    owner_login: &str,
    project_number: i64,
    options: &[(&str, &str)],
) -> Value {
    let options: Vec<Value> = options
        .iter()
        .map(|(name, id)| json!({"id": id, "name": name}))
        .collect();
    let owner_data = json!({
        "projectV2": {
            "id": "PROJECT_1",
            "fields": {
                "nodes": [{
                    "id": "STATUS_FIELD",
                    "name": "Status",
                    "options": options
                }]
            }
        }
    });
    let project_owner = match owner_type {
        GithubProjectOwnerType::Organization => {
            json!({"__typename": "Organization", "login": owner_login})
        }
        GithubProjectOwnerType::User => json!({"__typename": "User", "login": owner_login}),
    };
    json!({
        "data": {
            "organization": matches!(owner_type, GithubProjectOwnerType::Organization).then_some(owner_data.clone()),
            "user": matches!(owner_type, GithubProjectOwnerType::User).then_some(owner_data),
            "node": {
                "projectItems": {
                    "nodes": [{
                        "id": "ITEM_1",
                        "project": {
                            "id": "PROJECT_1",
                            "number": project_number,
                            "owner": project_owner
                        }
                    }]
                }
            }
        }
    })
}

fn ok(body: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: body.to_string(),
    }
}

struct HttpResponse {
    status: u16,
    body: String,
}

struct TestServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn new(responses: Vec<HttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                request_log.lock().unwrap().push(request);
                write_http_response(&mut stream, response);
            }
        });
        Self { url, requests }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0);
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_subslice(&buffer, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .or_else(|| {
                    headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                })
                .unwrap_or("0")
                .parse::<usize>()
                .unwrap();
            let needed = header_end + 4 + content_length;
            while buffer.len() < needed {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0);
                buffer.extend_from_slice(&chunk[..read]);
            }
            buffer.truncate(needed);
            return String::from_utf8(buffer).unwrap();
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http_response(stream: &mut std::net::TcpStream, response: HttpResponse) {
    let reason = if response.status == 200 {
        "OK"
    } else {
        "Error"
    };
    let reply = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.len(),
        response.body
    );
    stream.write_all(reply.as_bytes()).unwrap();
}
