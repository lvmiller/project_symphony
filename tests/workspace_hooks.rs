use std::fs;
use std::path::Path;
use std::process::Command;

use symphony::config::{
    HooksConfig, WorkspaceCleanupConfig, WorkspaceConfig, WorkspacePopulationConfig,
    WorkspacePopulationKind, WorkspacePopulationReusePolicy,
};
use symphony::error::SymphonyError;
use symphony::workspace::{WorkspaceManager, sanitize_workspace_key, source_workspace_key};
use tempfile::TempDir;

fn hooks_with_timeout(timeout_ms: u64) -> HooksConfig {
    HooksConfig {
        timeout_ms,
        ..HooksConfig::default()
    }
}

fn manager(root: &Path, hooks: HooksConfig) -> WorkspaceManager {
    manager_with_population(root, hooks, WorkspacePopulationConfig::default())
}

fn manager_with_population(
    root: &Path,
    hooks: HooksConfig,
    population: WorkspacePopulationConfig,
) -> WorkspaceManager {
    WorkspaceManager::new(
        &WorkspaceConfig {
            root: root.to_path_buf(),
            cleanup: WorkspaceCleanupConfig::default(),
            population,
        },
        hooks,
    )
    .expect("workspace manager should build")
}

fn git_population(repository: &Path) -> WorkspacePopulationConfig {
    WorkspacePopulationConfig {
        kind: WorkspacePopulationKind::Git,
        repository_url: Some(repository.display().to_string()),
        reference: None,
        branch: None,
        depth: None,
        reuse: WorkspacePopulationReusePolicy::Skip,
    }
}

fn git(path: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .output()
        .expect("git command should start");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_repository_text(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("repository text file")
        .replace("\r\n", "\n")
}

fn remote_repository() -> (TempDir, TempDir, TempDir) {
    let temp = TempDir::new().expect("tempdir");
    let source = TempDir::new_in(temp.path()).expect("source tempdir");
    let remote = TempDir::new_in(temp.path()).expect("remote tempdir");
    git(source.path(), &["init"]);
    git(
        source.path(),
        &["config", "user.email", "symphony@example.test"],
    );
    git(source.path(), &["config", "user.name", "Symphony Test"]);
    fs::write(source.path().join("README.md"), "initial\n").expect("initial repository file");
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-m", "initial"]);
    git(source.path(), &["branch", "-M", "main"]);
    git(remote.path(), &["init", "--bare"]);
    git(
        remote.path(),
        &["--git-dir=.", "symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(
        source.path(),
        &[
            "remote",
            "add",
            "origin",
            remote.path().to_str().expect("remote path"),
        ],
    );
    git(source.path(), &["push", "-u", "origin", "HEAD"]);
    (temp, source, remote)
}

fn assert_hook_error(error: SymphonyError, hook: &'static str, message_contains: &str) {
    match error {
        SymphonyError::Hook {
            hook: actual,
            message,
        } => {
            assert_eq!(actual, hook);
            assert!(
                message.contains(message_contains),
                "expected hook error message to contain {message_contains:?}, got {message:?}"
            );
        }
        other => panic!("expected hook error, got {other:?}"),
    }
}

fn assert_workspace_error(error: SymphonyError, message_contains: &str) {
    match error {
        SymphonyError::Workspace(message) => assert!(
            message.contains(message_contains),
            "expected workspace error message to contain {message_contains:?}, got {message:?}"
        ),
        other => panic!("expected workspace error, got {other:?}"),
    }
}

#[test]
fn sanitizes_workspace_keys_to_safe_path_segments() {
    assert_eq!(sanitize_workspace_key("issue/123"), "issue_123");
    assert_eq!(sanitize_workspace_key("A b$c"), "A_b_c");
    assert_eq!(sanitize_workspace_key(""), "_");
    assert_eq!(sanitize_workspace_key("."), "_");
    assert_eq!(sanitize_workspace_key(".."), "_");

    let temp = TempDir::new().expect("tempdir");
    let manager = manager(temp.path(), hooks_with_timeout(1_000));
    let (key, path) = manager
        .workspace_path_for_identifier("..")
        .expect("sanitized dot-dot identifier should stay contained");

    assert_eq!(key, "_");
    assert_eq!(path, manager.root().join("_"));
}

#[cfg(unix)]
#[tokio::test]
async fn existing_symlink_workspace_path_is_rejected_on_create() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let manager = manager(temp.path(), hooks_with_timeout(1_000));
    let (_, path) = manager
        .workspace_path_for_identifier("issue-1")
        .expect("path should be contained");
    fs::create_dir_all(temp.path()).expect("root create");
    symlink(outside.path(), &path).expect("workspace symlink create");

    let error = manager
        .create_for_identifier("issue-1")
        .await
        .expect_err("symlink workspace should fail safely");

    assert_workspace_error(error, "symlink");
    assert!(
        fs::symlink_metadata(&path)
            .expect("symlink remains")
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn creates_new_workspace_then_reuses_existing_directory() {
    let temp = TempDir::new().expect("tempdir");
    let manager = manager(temp.path(), hooks_with_timeout(1_000));

    let first = manager
        .create_for_identifier("issue/123")
        .await
        .expect("first create should succeed");
    assert_eq!(first.workspace_key, "issue_123");
    assert!(first.created_now);
    assert!(first.path.is_dir());

    let second = manager
        .create_for_identifier("issue/123")
        .await
        .expect("second create should reuse");
    assert_eq!(second.path, first.path);
    assert!(!second.created_now);
}

#[tokio::test]
async fn source_workspaces_are_namespaced_and_hooks_receive_source_id() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.after_create = Some("printf \"$SYMPHONY_SOURCE_ID\" > source_id".to_string());
    let manager = manager(temp.path(), hooks);

    let workspace = manager
        .create_for_source_identifier("api/service", "issue/123")
        .await
        .expect("source workspace should be created");

    assert_eq!(workspace.workspace_key, "api_service/issue_123");
    assert_eq!(
        source_workspace_key("api/service", "issue/123"),
        workspace.workspace_key
    );
    assert_eq!(
        workspace.path,
        fs::canonicalize(temp.path())
            .unwrap()
            .join("api_service")
            .join("issue_123")
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("source_id")).expect("source id marker"),
        "api/service"
    );
}

#[tokio::test]
async fn existing_non_directory_workspace_path_fails_without_replacing_it() {
    let temp = TempDir::new().expect("tempdir");
    let manager = manager(temp.path(), hooks_with_timeout(1_000));
    let (_, path) = manager
        .workspace_path_for_identifier("issue-1")
        .expect("path should be contained");
    fs::create_dir_all(temp.path()).expect("root create");
    fs::write(&path, "do not replace").expect("write sentinel file");

    let error = manager
        .create_for_identifier("issue-1")
        .await
        .expect_err("non-directory should fail safely");
    assert_workspace_error(error, "not a directory");
    assert_eq!(
        fs::read_to_string(&path).expect("sentinel file remains"),
        "do not replace"
    );
}

#[tokio::test]
async fn after_create_runs_only_for_new_workspace_with_workspace_cwd() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.after_create = Some("pwd > pwd.txt; printf x >> ../after_create_count".to_string());
    let manager = manager(temp.path(), hooks);

    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("first create should run after_create");
    let hook_cwd_marker = workspace.path.join("pwd.txt");
    assert!(
        hook_cwd_marker.is_file(),
        "hook relative output must be created in its configured workspace"
    );
    assert!(
        !fs::read_to_string(hook_cwd_marker)
            .expect("hook should write cwd marker")
            .trim()
            .is_empty(),
        "hook pwd output should not be empty"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("after_create_count")).expect("count marker"),
        "x"
    );

    let reused = manager
        .create_for_identifier("issue-1")
        .await
        .expect("reuse should succeed");
    assert!(!reused.created_now);
    assert_eq!(
        fs::read_to_string(temp.path().join("after_create_count")).expect("count marker"),
        "x"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn before_run_rejects_symlink_workspace_before_hook_runs() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.before_run = Some("printf ran > marker".to_string());
    let manager = manager(temp.path(), hooks);
    let (_, path) = manager
        .workspace_path_for_identifier("issue-1")
        .expect("path should be contained");
    fs::create_dir_all(temp.path()).expect("root create");
    symlink(outside.path(), &path).expect("workspace symlink create");

    let error = manager
        .before_run(&path)
        .await
        .expect_err("symlink workspace should fail before hook");

    assert_workspace_error(error, "symlink");
    assert!(!outside.path().join("marker").exists());
}

#[tokio::test]
async fn before_run_failure_is_fatal() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.before_run = Some("exit 7".to_string());
    let manager = manager(temp.path(), hooks);
    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("create should succeed");

    let error = manager
        .before_run(&workspace.path)
        .await
        .expect_err("before_run failure should be fatal");
    assert_hook_error(error, "before_run", "exit_status=");
}

#[tokio::test]
async fn after_run_failure_is_ignored_after_hook_runs() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.after_run = Some("printf ran > after_run_marker; exit 9".to_string());
    let manager = manager(temp.path(), hooks);
    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("create should succeed");

    manager.after_run_best_effort(&workspace.path).await;

    assert_eq!(
        fs::read_to_string(workspace.path.join("after_run_marker"))
            .expect("marker should be written"),
        "ran"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn remove_rejects_symlink_workspace_before_hook_or_delete() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.before_remove = Some("printf ran > marker".to_string());
    let manager = manager(temp.path(), hooks);
    let (_, path) = manager
        .workspace_path_for_identifier("issue-1")
        .expect("path should be contained");
    fs::create_dir_all(temp.path()).expect("root create");
    symlink(outside.path(), &path).expect("workspace symlink create");

    let error = manager
        .remove_for_identifier("issue-1")
        .await
        .expect_err("symlink workspace cleanup should fail before hook");

    assert_workspace_error(error, "symlink");
    assert!(outside.path().exists());
    assert!(!outside.path().join("marker").exists());
    assert!(
        fs::symlink_metadata(&path)
            .expect("symlink remains")
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn before_remove_runs_before_directory_is_removed() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.before_remove = Some("test -d . && printf ran > ../before_remove_marker".to_string());
    let manager = manager(temp.path(), hooks);
    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("create should succeed");

    manager
        .remove_for_identifier("issue-1")
        .await
        .expect("remove should succeed");

    assert!(!workspace.path.exists());
    assert_eq!(
        fs::read_to_string(temp.path().join("before_remove_marker")).expect("remove marker"),
        "ran"
    );
}

#[tokio::test]
async fn fatal_hook_timeout_returns_error() {
    let temp = TempDir::new().expect("tempdir");
    let mut hooks = hooks_with_timeout(10);
    hooks.before_run = Some("sleep 1".to_string());
    let manager = manager(temp.path(), hooks);
    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("create should succeed");

    let error = manager
        .before_run(&workspace.path)
        .await
        .expect_err("timeout should be fatal");
    assert_hook_error(error, "before_run", "timeout after 10 ms");
}

#[tokio::test]
async fn git_population_clones_before_after_create_hook() {
    let (_repository_root, _source, remote) = remote_repository();
    let workspaces = TempDir::new().expect("workspace root");
    let mut hooks = hooks_with_timeout(1_000);
    hooks.after_create = Some("test -d .git && printf ran > after_create_marker".to_string());
    let manager = manager_with_population(workspaces.path(), hooks, git_population(remote.path()));

    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("git population should clone a new workspace");

    assert!(workspace.created_now);
    assert_eq!(
        read_repository_text(&workspace.path.join("README.md")),
        "initial\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("after_create_marker")).expect("hook marker"),
        "ran"
    );
}

#[tokio::test]
async fn git_population_reuse_skip_preserves_existing_workspace() {
    let (_repository_root, source, remote) = remote_repository();
    let workspaces = TempDir::new().expect("workspace root");
    let manager = manager_with_population(
        workspaces.path(),
        hooks_with_timeout(1_000),
        git_population(remote.path()),
    );

    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("initial clone should succeed");
    fs::write(workspace.path.join("local-sentinel"), "keep me").expect("local sentinel");
    fs::write(source.path().join("README.md"), "remote update\n").expect("remote update");
    git(source.path(), &["add", "README.md"]);
    git(source.path(), &["commit", "-m", "remote update"]);
    git(source.path(), &["push"]);

    let reused = manager
        .create_for_identifier("issue-1")
        .await
        .expect("reuse skip should succeed");

    assert!(!reused.created_now);
    assert_eq!(
        read_repository_text(&reused.path.join("README.md")),
        "initial\n"
    );
    assert_eq!(
        fs::read_to_string(reused.path.join("local-sentinel")).expect("sentinel"),
        "keep me"
    );
}

#[tokio::test]
async fn git_population_fast_forward_syncs_reused_checkout() {
    let (_repository_root, source, remote) = remote_repository();
    let workspaces = TempDir::new().expect("workspace root");
    let mut population = git_population(remote.path());
    population.reuse = WorkspacePopulationReusePolicy::FetchFfOnly;
    let manager = manager_with_population(workspaces.path(), hooks_with_timeout(1_000), population);

    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("initial clone should succeed");
    fs::write(source.path().join("README.md"), "remote update\n").expect("remote update");
    git(source.path(), &["add", "README.md"]);
    git(source.path(), &["commit", "-m", "remote update"]);
    git(source.path(), &["push"]);

    manager
        .create_for_identifier("issue-1")
        .await
        .expect("fast-forward sync should succeed");
    assert_eq!(
        read_repository_text(&workspace.path.join("README.md")),
        "remote update\n"
    );
}

#[tokio::test]
async fn git_population_fast_forward_rejects_divergence_without_removing_workspace() {
    let (_repository_root, source, remote) = remote_repository();
    let workspaces = TempDir::new().expect("workspace root");
    let mut population = git_population(remote.path());
    population.reuse = WorkspacePopulationReusePolicy::FetchFfOnly;
    let manager = manager_with_population(workspaces.path(), hooks_with_timeout(1_000), population);

    let workspace = manager
        .create_for_identifier("issue-1")
        .await
        .expect("initial clone should succeed");
    fs::write(source.path().join("README.md"), "remote update\n").expect("remote update");
    git(source.path(), &["add", "README.md"]);
    git(source.path(), &["commit", "-m", "remote update"]);
    git(source.path(), &["push"]);

    git(
        &workspace.path,
        &["config", "user.email", "symphony@example.test"],
    );
    git(&workspace.path, &["config", "user.name", "Symphony Test"]);
    fs::write(workspace.path.join("local-sentinel"), "keep me").expect("local change");
    git(&workspace.path, &["add", "local-sentinel"]);
    git(&workspace.path, &["commit", "-m", "local change"]);

    let error = manager
        .create_for_identifier("issue-1")
        .await
        .expect_err("divergent checkout must not be reset");

    assert_workspace_error(error, "fast-forward sync");
    assert_eq!(
        fs::read_to_string(workspace.path.join("local-sentinel")).expect("local file preserved"),
        "keep me"
    );
    assert!(workspace.path.is_dir(), "reused workspace must remain");
}

#[tokio::test]
async fn failed_new_git_population_cleans_partial_workspace_and_redacts_credentials() {
    let workspaces = TempDir::new().expect("workspace root");
    let population = WorkspacePopulationConfig {
        kind: WorkspacePopulationKind::Git,
        repository_url: Some("https://user:super-secret@127.0.0.1:1/not-a-repo".to_string()),
        reference: None,
        branch: None,
        depth: None,
        reuse: WorkspacePopulationReusePolicy::Skip,
    };
    let manager = manager_with_population(workspaces.path(), hooks_with_timeout(1_000), population);
    let (_, workspace_path) = manager
        .workspace_path_for_identifier("issue-1")
        .expect("contained workspace path");

    let error = manager
        .create_for_identifier("issue-1")
        .await
        .expect_err("failed clone should fail workspace creation");

    let message = error.to_string();
    assert_workspace_error(error, "git clone failed");
    assert!(!message.contains("super-secret"));
    assert!(
        !workspace_path.exists(),
        "new partial workspace must be removed"
    );
}
