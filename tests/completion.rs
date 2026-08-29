use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use symphony::completion::{CompletionMutation, GitHubCompletionClient};
use symphony::config::{
    AgentConfig, CodexConfig, CompletionConfig, DirectCommitCompletionConfig, EffectiveConfig,
    GithubConfig, GithubProjectOwnerType, GithubRepositoryConfig, HooksConfig, PollingConfig,
    ServerConfig, SourceConfig, TrackerConfig, WorkspaceCleanupConfig, WorkspaceConfig,
};
use symphony::domain::Issue;

#[tokio::test]
async fn medium_issue_commits_to_main_and_moves_to_done() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nchanged\n").unwrap();

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(result.severity.as_deref(), Some("Medium"));
    assert!(result.commit_sha.as_deref().unwrap().len() >= 40);
    assert!(result.skipped_reason.is_none());
    let main_readme = git_show(&remote, "refs/heads/main:README.md");
    assert_eq!(main_readme, "initial\nchanged\n");
    let commit_message = git_show(&remote, "refs/heads/main^{commit}");
    assert!(commit_message.contains("lvmiller/project_symphony#1: [Medium] Fix allocator"));
    assert!(commit_message.contains("Refs #1"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let lookup_body: Value = serde_json::from_str(request_body(&requests[0])).unwrap();
    assert!(
        lookup_body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyCompletionProject")
    );
    assert_eq!(lookup_body["variables"]["projectOwnerLogin"], "lvmiller");
    assert_eq!(lookup_body["variables"]["projectNumber"], 2);
    assert_eq!(lookup_body["variables"]["isOrganization"], false);
    assert_eq!(lookup_body["variables"]["isUser"], true);

    let mutation_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert!(
        mutation_body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyUpdateCompletionStatus")
    );
    assert_eq!(mutation_body["variables"]["projectId"], "PROJECT_2");
    assert_eq!(mutation_body["variables"]["itemId"], "ITEM_1");
    assert_eq!(mutation_body["variables"]["fieldId"], "STATUS_FIELD");
    assert_eq!(mutation_body["variables"]["optionId"], "DONE_OPTION");
}

#[tokio::test]
async fn low_issue_commits_to_main_and_moves_to_done() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nlow risk fix\n").unwrap();

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Low] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(result.severity.as_deref(), Some("Low"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let mutation_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert_eq!(mutation_body["variables"]["optionId"], "DONE_OPTION");
}

#[tokio::test]
async fn high_issue_commits_to_main_and_moves_to_review() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\ncritical fix\n").unwrap();

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[High] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("In review"));
    assert_eq!(result.severity.as_deref(), Some("High"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let mutation_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert_eq!(mutation_body["variables"]["optionId"], "REVIEW_OPTION");
}

#[tokio::test]
async fn dry_run_returns_read_only_plan_without_changing_worktree_remote_or_status() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\ndry run change\n").unwrap();
    let remote_head_before = git_rev_parse(&remote, "refs/heads/main");

    let server = TestServer::new(Vec::new());
    let mut config = config(format!("{}/graphql", server.url()));
    config.completion.direct_commit.dry_run = true;
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.commit_sha, None);
    assert_eq!(result.moved_to_state, None);
    assert_eq!(result.severity.as_deref(), Some("Medium"));
    assert!(result.partial_failure.is_none());
    let plan = result.plan.as_ref().expect("dry run must return a plan");
    assert_eq!(plan.severity, "Medium");
    assert_eq!(plan.target_state, "Done");
    assert_eq!(plan.detected_changes, vec![" M README.md"]);
    assert_eq!(
        plan.commit_title,
        "lvmiller/project_symphony#1: [Medium] Fix allocator"
    );
    assert!(plan.commit_body.contains("Refs #1"));
    assert!(!plan.rebase_required);
    assert_eq!(
        plan.planned_mutations,
        vec![
            CompletionMutation::StageAllChanges,
            CompletionMutation::Commit {
                title: "lvmiller/project_symphony#1: [Medium] Fix allocator".to_string(),
                body: plan.commit_body.clone(),
            },
            CompletionMutation::FetchBaseBranch {
                base_branch: "main".to_string(),
            },
            CompletionMutation::PushBaseBranch {
                base_branch: "main".to_string(),
            },
            CompletionMutation::MoveIssueToState {
                target_state: "Done".to_string(),
            },
        ]
    );
    assert_eq!(
        String::from_utf8(
            Command::new("git")
                .args(["status", "--porcelain=v1"])
                .current_dir(&work)
                .output()
                .unwrap()
                .stdout
        )
        .unwrap(),
        " M README.md\n"
    );
    assert_eq!(
        git_rev_parse(&remote, "refs/heads/main"),
        remote_head_before
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn dry_run_missing_severity_fails_before_any_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\ndry run change\n").unwrap();
    let remote_head_before = git_rev_parse(&remote, "refs/heads/main");

    let server = TestServer::new(Vec::new());
    let mut config = config(format!("{}/graphql", server.url()));
    config.completion.direct_commit.dry_run = true;
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();

    let error = client
        .complete_issue(&issue("Fix allocator"), &work)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("missing_issue_severity"));
    assert_eq!(
        git_rev_parse(&remote, "refs/heads/main"),
        remote_head_before
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn status_handoff_failure_after_push_returns_partial_failure_details() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\npushed change\n").unwrap();
    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        HttpResponse {
            status: 500,
            body: "{\"message\":\"status handoff unavailable\"}".to_string(),
        },
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state, None);
    assert_eq!(result.severity.as_deref(), Some("Medium"));
    let partial = result
        .partial_failure
        .as_ref()
        .expect("pushed status handoff failure must be reported");
    assert_eq!(partial.target_state, "Done");
    assert_eq!(
        partial.pushed_commit_sha,
        git_rev_parse(&remote, "refs/heads/main")
    );
    assert_eq!(
        result.commit_sha.as_deref(),
        Some(partial.pushed_commit_sha.as_str())
    );
    assert!(partial.message.contains("github_status"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn precommitted_worker_changes_push_to_main_and_move_status() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    create_working_repo(temp.path(), &remote);
    let work = temp.path().join("precommitted");
    run_git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    std::fs::write(work.join("README.md"), "initial\nagent committed change\n").unwrap();
    run_git(&work, &["add", "README.md"]);
    run_git(
        &work,
        &[
            "-c",
            "user.name=Agent",
            "-c",
            "user.email=agent@example.test",
            "commit",
            "-m",
            "agent committed change",
        ],
    );

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(result.severity.as_deref(), Some("Medium"));
    let main_readme = git_show(&remote, "refs/heads/main:README.md");
    assert_eq!(main_readme, "initial\nagent committed change\n");
    let commit_message = git_show(&remote, "refs/heads/main^{commit}");
    assert!(commit_message.contains("agent committed change"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn precommitted_worker_changes_rebase_when_remote_main_advanced() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    create_working_repo(temp.path(), &remote);
    let work = temp.path().join("worker");
    let other = temp.path().join("other");
    run_git(
        temp.path(),
        &[
            "clone",
            "--depth",
            "1",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    run_git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("REMOTE.md"), "remote change\n").unwrap();
    run_git(&other, &["add", "REMOTE.md"]);
    run_git(
        &other,
        &[
            "-c",
            "user.name=Remote",
            "-c",
            "user.email=remote@example.test",
            "commit",
            "-m",
            "remote main advanced",
        ],
    );
    run_git(&other, &["push", "origin", "main"]);
    std::fs::write(work.join("WORKER.md"), "worker change\n").unwrap();
    run_git(&work, &["add", "WORKER.md"]);
    run_git(
        &work,
        &[
            "-c",
            "user.name=Agent",
            "-c",
            "user.email=agent@example.test",
            "commit",
            "-m",
            "agent committed change",
        ],
    );

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(result.severity.as_deref(), Some("Medium"));
    assert_eq!(
        git_show(&remote, "refs/heads/main:REMOTE.md"),
        "remote change\n"
    );
    assert_eq!(
        git_show(&remote, "refs/heads/main:WORKER.md"),
        "worker change\n"
    );
    let commit_message = git_show(&remote, "refs/heads/main^{commit}");
    assert!(commit_message.contains("agent committed change"));
    let parent_message = git_show(&remote, "refs/heads/main^1^{commit}");
    assert!(parent_message.contains("remote main advanced"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn dirty_rebase_retry_commits_late_workspace_changes() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let seed = create_working_repo(temp.path(), &remote);
    std::fs::create_dir_all(seed.join("src")).unwrap();
    std::fs::write(seed.join("src/service.rs"), "base\n").unwrap();
    run_git(&seed, &["add", "src/service.rs"]);
    run_git(
        &seed,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.test",
            "commit",
            "-m",
            "add service",
        ],
    );
    run_git(&seed, &["push", "origin", "main"]);
    let work = temp.path().join("worker");
    let other = temp.path().join("other");
    run_git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    run_git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("REMOTE.md"), "remote change\n").unwrap();
    run_git(&other, &["add", "REMOTE.md"]);
    run_git(
        &other,
        &[
            "-c",
            "user.name=Remote",
            "-c",
            "user.email=remote@example.test",
            "commit",
            "-m",
            "remote main advanced",
        ],
    );
    run_git(&other, &["push", "origin", "main"]);
    std::fs::write(work.join("WORKER.md"), "worker change\n").unwrap();
    run_git(&work, &["add", "WORKER.md"]);
    run_git(
        &work,
        &[
            "-c",
            "user.name=Agent",
            "-c",
            "user.email=agent@example.test",
            "commit",
            "-m",
            "agent committed change",
        ],
    );
    let pre_rebase_hook = work.join(".git/hooks/pre-rebase");
    let hook_marker = temp.path().join("pre-rebase-ran");
    std::fs::write(
        &pre_rebase_hook,
        format!(
            "#!/bin/sh\nif [ ! -f {} ]; then printf 'late change\\n' >> src/service.rs; touch {}; fi\n",
            shell_quote(&hook_marker),
            shell_quote(&hook_marker)
        ),
    )
    .unwrap();
    make_executable(&pre_rebase_hook);

    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("Done", "DONE_OPTION"),
            ("In review", "REVIEW_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(
        git_show(&remote, "refs/heads/main:REMOTE.md"),
        "remote change\n"
    );
    assert_eq!(
        git_show(&remote, "refs/heads/main:WORKER.md"),
        "worker change\n"
    );
    assert_eq!(
        git_show(&remote, "refs/heads/main:src/service.rs"),
        "base\nlate change\n"
    );
    let commit_message = git_show(&remote, "refs/heads/main^{commit}");
    assert!(commit_message.contains("lvmiller/project_symphony#1"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn mark_issue_started_moves_ready_issue_to_in_progress() {
    let server = TestServer::new(vec![
        ok(project_status_lookup(&[
            ("Ready", "READY_OPTION"),
            ("In progress", "PROGRESS_OPTION"),
            ("Done", "DONE_OPTION"),
        ])),
        ok(json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM_1"}}}})),
    ]);
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let moved = client.mark_issue_started(&issue).await.unwrap();

    assert_eq!(moved.as_deref(), Some("In progress"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let mutation_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert_eq!(mutation_body["variables"]["optionId"], "PROGRESS_OPTION");
}

#[tokio::test]
async fn mark_issue_started_skips_already_started_issue() {
    let server = TestServer::new(Vec::new());
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let mut issue = issue("[Medium] Fix allocator");
    issue.state = "In progress".to_string();

    let moved = client.mark_issue_started(&issue).await.unwrap();

    assert!(moved.is_none());
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn completion_requests_timeout_after_thirty_seconds() {
    let server = DelayedServer::new(Duration::from_secs(35));
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("[Medium] Fix allocator");

    let err = tokio::time::timeout(Duration::from_secs(31), client.mark_issue_started(&issue))
        .await
        .expect("completion request must be bounded to 30 seconds")
        .unwrap_err();

    assert!(err.to_string().contains("github_transport"));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn missing_title_severity_does_not_commit_or_move_status() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nchanged\n").unwrap();

    let server = TestServer::new(Vec::new());
    let config = config(format!("{}/graphql", server.url()));
    let client = GitHubCompletionClient::new(&config).unwrap().unwrap();
    let issue = issue("Fix allocator");

    let error = client.complete_issue(&issue, &work).await.unwrap_err();

    let message = error.to_string();
    assert!(message.contains("missing_issue_severity"));
    assert!(server.requests().is_empty());
    let main_readme = git_show(&remote, "refs/heads/main:README.md");
    assert_eq!(main_readme, "initial\n");
}

fn create_working_repo(root: &Path, remote: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    run_git(root, &["init", "--bare", remote.to_str().unwrap()]);
    run_git(&work, &["init"]);
    std::fs::write(work.join("README.md"), "initial\n").unwrap();
    run_git(&work, &["add", "README.md"]);
    run_git(
        &work,
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
    run_git(&work, &["branch", "-M", "main"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "origin", "main"]);
    work
}

fn config(endpoint: String) -> EffectiveConfig {
    EffectiveConfig {
        workflow_path: "WORKFLOW.md".into(),
        workflow_dir: ".".into(),
        prompt_template: String::new(),
        source: SourceConfig {
            id: "default".to_string(),
        },
        tracker: TrackerConfig {
            kind: "github".to_string(),
            endpoint,
            api_key: Some("test-token".to_string()),
            active_states: vec!["Ready".to_string(), "In progress".to_string()],
            terminal_states: vec!["Done".to_string()],
            github: Some(GithubConfig {
                repository_owner: "lvmiller".to_string(),
                repository_name: "project_symphony".to_string(),
                repositories: vec![GithubRepositoryConfig {
                    owner: "lvmiller".to_string(),
                    name: "project_symphony".to_string(),
                }],
                project_owner_type: GithubProjectOwnerType::User,
                project_owner_login: "lvmiller".to_string(),
                project_number: 2,
                status_field_name: "Status".to_string(),
                priority_field_name: Some("Priority".to_string()),
                blocker_field_name: None,
                blocker_label_prefix: None,
                priority_labels: BTreeMap::new(),
            }),
        },
        polling: PollingConfig { interval_ms: 1_000 },
        workspace: WorkspaceConfig {
            root: "workspaces".into(),
            cleanup: WorkspaceCleanupConfig::default(),
            population: Default::default(),
        },
        hooks: HooksConfig::default(),
        agent: AgentConfig {
            max_concurrent_agents: 1,
            max_turns: 1,
            max_retry_backoff_ms: 30_000,
            max_concurrent_agents_by_state: BTreeMap::new(),
        },
        codex: CodexConfig {
            command: "codex app-server".to_string(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: 60_000,
            read_timeout_ms: 5_000,
            stall_timeout_ms: 300_000,
        },
        completion: CompletionConfig {
            direct_commit: DirectCommitCompletionConfig {
                enabled: true,
                dry_run: false,
                base_branch: "main".to_string(),
                high_review_state: "In review".to_string(),
                auto_approved_state: "Done".to_string(),
                started_state: Some("In progress".to_string()),
                commit_author_name: "Symphony".to_string(),
                commit_author_email: "symphony@users.noreply.github.com".to_string(),
            },
        },
        server: ServerConfig::default(),
    }
}

fn issue(title: &str) -> Issue {
    Issue {
        id: "ISSUE_NODE_1".to_string(),
        identifier: "lvmiller/project_symphony#1".to_string(),
        title: title.to_string(),
        description: Some("body".to_string()),
        priority: None,
        state: "Ready".to_string(),
        branch_name: None,
        url: Some("https://github.com/lvmiller/project_symphony/issues/1".to_string()),
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    }
}

fn project_status_lookup(options: &[(&str, &str)]) -> Value {
    let options: Vec<Value> = options
        .iter()
        .map(|(name, id)| json!({"id": id, "name": name}))
        .collect();
    json!({
        "data": {
            "organization": null,
            "user": {
                "projectV2": {
                    "id": "PROJECT_2",
                    "fields": {
                        "nodes": [{
                            "id": "STATUS_FIELD",
                            "name": "Status",
                            "options": options
                        }]
                    }
                }
            },
            "node": {
                "projectItems": {
                    "nodes": [{
                        "id": "ITEM_1",
                        "project": {
                            "id": "PROJECT_2",
                            "number": 2,
                            "owner": {"__typename": "User", "login": "lvmiller"}
                        }
                    }]
                }
            }
        }
    })
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

fn git_rev_parse(git_dir: &Path, revision: &str) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", revision])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git rev-parse {:?} failed\nstdout={}\nstderr={}",
        revision,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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

struct DelayedServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl DelayedServer {
    fn new(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            request_log
                .lock()
                .unwrap()
                .push(read_http_request(&mut stream));
            thread::sleep(delay);
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

fn request_body(request: &str) -> &str {
    request.split("\r\n\r\n").nth(1).unwrap()
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
