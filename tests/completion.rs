use async_trait::async_trait;
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use symphony::completion::{CompletionMutation, DirectCommitCompletion};
use symphony::config::{
    AgentConfig, CodexConfig, CompletionConfig, DirectCommitCompletionConfig, EffectiveConfig,
    GithubConfig, GithubProjectOwnerType, GithubRepositoryConfig, HooksConfig, PollingConfig,
    ServerConfig, SourceConfig, TrackerConfig, WorkspaceCleanupConfig, WorkspaceConfig,
};
use symphony::domain::Issue;
use symphony::error::{Result, SymphonyError};
use symphony::tracker::TrackerWriter;

#[tokio::test]
async fn completion_push_ignores_repository_and_configured_hooks_and_moves_done() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nchanged\n").unwrap();
    let marker = temp.path().join("hook-ran");
    let alternate_hooks = temp.path().join("alternate-hooks");
    std::fs::create_dir(&alternate_hooks).unwrap();
    for path in [
        work.join(".git/hooks/pre-push"),
        alternate_hooks.join("pre-push"),
    ] {
        std::fs::write(
            &path,
            format!("#!/bin/sh\ntouch {}\n", shell_quote(&marker)),
        )
        .unwrap();
        make_executable(&path);
    }
    run_git(
        &work,
        &[
            "config",
            "core.hooksPath",
            alternate_hooks.to_str().unwrap(),
        ],
    );

    let writer = Arc::new(FakeWriter::default());
    let client = completion(&config(), writer.clone());
    let issue = issue("[Medium] Fix allocator");
    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(writer.moves(), vec![(issue.id, "Done".to_string())]);
    assert!(
        !marker.exists(),
        "credentialed push must not execute any hook"
    );
    assert_eq!(
        git_show(&remote, "refs/heads/main:README.md"),
        "initial\nchanged\n"
    );
}

#[test]
fn enabled_completion_requires_a_tracker_writer() {
    let error = DirectCommitCompletion::new(&config(), None)
        .err()
        .expect("missing writer must be rejected");

    assert!(error.to_string().contains("completion_writer_unavailable"));
}

#[tokio::test]
async fn started_state_transition_uses_tracker_writer() {
    let writer = Arc::new(FakeWriter::default());
    let client = completion(&config(), writer.clone());
    let issue = issue("[Medium] Fix allocator");

    let moved = client.mark_issue_started(&issue).await.unwrap();

    assert_eq!(moved.as_deref(), Some("In progress"));
    assert_eq!(writer.moves(), vec![(issue.id, "In progress".to_string())]);
}

#[tokio::test]
async fn high_severity_completion_moves_review_through_writer() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nhigh risk fix\n").unwrap();
    let writer = Arc::new(FakeWriter::default());
    let client = completion(&config(), writer.clone());
    let issue = issue("[High] Fix allocator");

    let result = client.complete_issue(&issue, &work).await.unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("In review"));
    assert_eq!(writer.moves(), vec![(issue.id, "In review".to_string())]);
}

#[tokio::test]
async fn dry_run_is_read_only_and_does_not_write_tracker() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    let remote_head_before = git_rev_parse(&remote, "refs/heads/main");
    std::fs::write(work.join("README.md"), "initial\ndry run change\n").unwrap();
    let mut config = config();
    config.completion.direct_commit.dry_run = true;
    let writer = Arc::new(FakeWriter::default());

    let result = completion(&config, writer.clone())
        .complete_issue(&issue("[Medium] Fix allocator"), &work)
        .await
        .unwrap();

    let plan = result.plan.expect("dry run returns a plan");
    assert_eq!(plan.target_state, "Done");
    assert_eq!(
        plan.planned_mutations,
        vec![
            CompletionMutation::StageAllChanges,
            CompletionMutation::Commit {
                title: plan.commit_title.clone(),
                body: plan.commit_body.clone()
            },
            CompletionMutation::FetchBaseBranch {
                base_branch: "main".to_string()
            },
            CompletionMutation::PushBaseBranch {
                base_branch: "main".to_string()
            },
            CompletionMutation::MoveIssueToState {
                target_state: "Done".to_string()
            },
        ]
    );
    assert_eq!(
        git_rev_parse(&remote, "refs/heads/main"),
        remote_head_before
    );
    assert!(writer.moves().is_empty());
    assert_eq!(
        git_output(&work, &["status", "--porcelain=v1"]),
        " M README.md\n"
    );
}

#[tokio::test]
async fn status_handoff_failure_after_push_is_partial_failure() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\npushed change\n").unwrap();
    let writer = Arc::new(FakeWriter::failing("handoff unavailable"));

    let result = completion(&config(), writer)
        .complete_issue(&issue("[Medium] Fix allocator"), &work)
        .await
        .unwrap();

    let partial = result
        .partial_failure
        .expect("push must retain handoff failure");
    assert_eq!(partial.target_state, "Done");
    assert_eq!(
        partial.pushed_commit_sha,
        git_rev_parse(&remote, "refs/heads/main")
    );
    assert!(partial.message.contains("handoff unavailable"));
}

#[tokio::test]
async fn precommitted_changes_rebase_before_push_and_move_state() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    create_working_repo(temp.path(), &remote);
    let worker = temp.path().join("worker");
    let other = temp.path().join("other");
    clone_main(temp.path(), &remote, &worker);
    clone_main(temp.path(), &remote, &other);
    std::fs::write(other.join("REMOTE.md"), "remote change\n").expect("write remote change");
    commit_all(&other, "remote main advanced");
    run_git(&other, &["push", "origin", "main"]);
    std::fs::write(worker.join("WORKER.md"), "worker change\n").expect("write worker change");
    commit_all(&worker, "worker change");
    let writer = Arc::new(FakeWriter::default());

    let result = completion(&config(), writer.clone())
        .complete_issue(&issue("[Medium] Fix allocator"), &worker)
        .await
        .unwrap();

    assert_eq!(result.moved_to_state.as_deref(), Some("Done"));
    assert_eq!(
        git_show(&remote, "refs/heads/main:REMOTE.md"),
        "remote change\n"
    );
    assert_eq!(
        git_show(&remote, "refs/heads/main:WORKER.md"),
        "worker change\n"
    );
    assert_eq!(writer.moves().len(), 1);
}

#[tokio::test]
async fn missing_severity_fails_before_commit_or_state_write() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = create_working_repo(temp.path(), &remote);
    std::fs::write(work.join("README.md"), "initial\nchanged\n").unwrap();
    let writer = Arc::new(FakeWriter::default());

    let error = completion(&config(), writer.clone())
        .complete_issue(&issue("Fix allocator"), &work)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("missing_issue_severity"));
    assert_eq!(git_show(&remote, "refs/heads/main:README.md"), "initial\n");
    assert!(writer.moves().is_empty());
}

#[derive(Default)]
struct FakeWriter {
    moves: Mutex<Vec<(String, String)>>,
    failure: Option<&'static str>,
}

impl FakeWriter {
    fn failing(message: &'static str) -> Self {
        Self {
            moves: Mutex::new(Vec::new()),
            failure: Some(message),
        }
    }
    fn moves(&self) -> Vec<(String, String)> {
        self.moves.lock().unwrap().clone()
    }
}

#[async_trait]
impl TrackerWriter for FakeWriter {
    async fn move_issue_to_state(&self, issue: &Issue, target_state: &str) -> Result<()> {
        self.moves
            .lock()
            .unwrap()
            .push((issue.id.clone(), target_state.to_string()));
        match self.failure {
            Some(message) => Err(SymphonyError::tracker("fake_writer", message)),
            None => Ok(()),
        }
    }
}

fn completion(config: &EffectiveConfig, writer: Arc<FakeWriter>) -> DirectCommitCompletion {
    let writer: Arc<dyn TrackerWriter> = writer;
    DirectCommitCompletion::new(config, Some(writer))
        .unwrap()
        .unwrap()
}

fn config() -> EffectiveConfig {
    EffectiveConfig {
        workflow_path: "WORKFLOW.md".into(),
        workflow_dir: ".".into(),
        prompt_template: String::new(),
        source: SourceConfig {
            id: "default".to_string(),
        },
        tracker: TrackerConfig {
            kind: "github".to_string(),
            endpoint: "https://api.github.test/graphql".to_string(),
            api_key: Some("test-token".to_string()),
            active_states: vec!["Ready".to_string()],
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
                priority_field_name: None,
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

fn create_working_repo(root: &Path, remote: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    run_git(root, &["init", "--bare", remote.to_str().unwrap()]);
    run_git(&work, &["init"]);
    std::fs::write(work.join("README.md"), "initial\n").unwrap();
    commit_all(&work, "initial");
    run_git(&work, &["branch", "-M", "main"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "origin", "main"]);
    work
}
fn clone_main(root: &Path, remote: &Path, work: &Path) {
    run_git(
        root,
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
}
fn commit_all(work: &Path, message: &str) {
    run_git(work, &["add", "-A"]);
    run_git(
        work,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-m",
            message,
        ],
    );
}
fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn git_output(cwd: &Path, args: &[&str]) -> String {
    String::from_utf8(
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}
fn git_show(git_dir: &Path, spec: &str) -> String {
    git_output(
        git_dir,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "show",
            "--format=%B",
            spec,
        ],
    )
}
fn git_rev_parse(git_dir: &Path, revision: &str) -> String {
    git_output(
        git_dir,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "rev-parse",
            revision,
        ],
    )
    .trim()
    .to_string()
}
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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
