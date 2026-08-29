use std::env;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use symphony::config::{
    ConfigReloader, ConfigSetReloader, DEFAULT_GITHUB_ENDPOINT, EffectiveConfig,
    WorkspaceCleanupAfterSuccess,
};
use symphony::error::SymphonyError;
use symphony::workflow::{load_workflow, parse_workflow, select_workflow_path};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn valid_workflow(root: Option<&str>) -> String {
    let workspace = root
        .map(|root| format!("workspace:\n  root: {root}\n"))
        .unwrap_or_default();
    format!(
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_type: organization\n    owner_login: octo\n    number: 7\n{workspace}---\nPrompt for {{{{ issue.identifier }}}}\n"
    )
}

fn write_workflow(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    std::fs::write(&path, contents).unwrap();
    path
}

fn load_from_path(path: PathBuf) -> EffectiveConfig {
    EffectiveConfig::load(Some(path)).unwrap()
}

#[test]
fn select_workflow_path_uses_explicit_path_before_cwd_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let previous = env::current_dir().unwrap();
    env::set_current_dir(temp.path()).unwrap();
    let cwd = env::current_dir().unwrap();

    let explicit_relative = select_workflow_path(Some(PathBuf::from("custom.md"))).unwrap();
    let default = select_workflow_path(None).unwrap();
    let explicit_absolute = select_workflow_path(Some(temp.path().join("absolute.md"))).unwrap();

    env::set_current_dir(previous).unwrap();

    assert_eq!(explicit_relative, cwd.join("custom.md"));
    assert_eq!(default, cwd.join("WORKFLOW.md"));
    assert_eq!(explicit_absolute, temp.path().join("absolute.md"));
}

#[test]
fn parse_yaml_front_matter_and_reject_non_map_front_matter() {
    let path = PathBuf::from("WORKFLOW.md");
    let workflow = parse_workflow(
        path.clone(),
        "---\ntracker:\n  kind: github\n---\n\n  Body text  \n",
    )
    .unwrap();
    assert_eq!(workflow.prompt_template, "Body text");
    let tracker_key = serde_yaml::Value::String("tracker".into());
    assert!(workflow.config.contains_key(&tracker_key));

    let error = parse_workflow(path, "---\n- not\n- a\n- map\n---\nBody").unwrap_err();
    assert!(matches!(
        error,
        SymphonyError::WorkflowFrontMatterNotMap { .. }
    ));
}

#[test]
fn load_workflow_rejects_directory_paths() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("WORKFLOW.md");
    std::fs::create_dir(&path).unwrap();

    let error = load_workflow(&path).unwrap_err();

    match error {
        SymphonyError::WorkflowPathNotFile { path: actual } => assert_eq!(actual, path),
        other => panic!("expected workflow_path_not_file, got {other}"),
    }
}

#[test]
fn github_defaults_and_default_token_indirection_are_applied() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), &valid_workflow(None));

    let config = load_from_path(path);
    config.validate_dispatch().unwrap();

    assert_eq!(config.tracker.kind, "github");
    assert_eq!(config.tracker.endpoint, DEFAULT_GITHUB_ENDPOINT);
    assert_eq!(config.tracker.api_key.as_deref(), Some("unit-token"));
    assert_eq!(config.tracker.active_states, ["Todo", "In Progress"]);
    assert_eq!(
        config.tracker.terminal_states,
        ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"]
    );
    assert_eq!(config.polling.interval_ms, 30_000);
    assert_eq!(config.hooks.timeout_ms, 60_000);
    assert_eq!(config.agent.max_concurrent_agents, 10);
    assert_eq!(config.agent.max_turns, 20);
    assert_eq!(config.agent.max_retry_backoff_ms, 300_000);
    assert!(config.agent.max_concurrent_agents_by_state.is_empty());
    assert_eq!(config.codex.command, "codex app-server");
    assert_eq!(config.codex.turn_timeout_ms, 3_600_000);
    assert_eq!(config.codex.read_timeout_ms, 5_000);
    assert_eq!(config.codex.stall_timeout_ms, 300_000);
    assert!(config.workspace.root.ends_with("symphony_workspaces"));
    assert_eq!(
        config.workspace.cleanup.after_success,
        WorkspaceCleanupAfterSuccess::Committed
    );
    assert!(!config.completion.direct_commit.enabled);
    assert_eq!(config.completion.direct_commit.base_branch, "main");
    assert_eq!(
        config.completion.direct_commit.high_review_state,
        "In review"
    );
    assert_eq!(config.completion.direct_commit.auto_approved_state, "Done");
    assert_eq!(config.completion.direct_commit.started_state, None);
}

#[test]
fn workspace_cleanup_policy_is_parsed_and_validated() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let valid = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nworkspace:\n  cleanup:\n    after_success: never\n---\nPrompt\n",
    );
    let config = load_from_path(valid);
    assert_eq!(
        config.workspace.cleanup.after_success,
        WorkspaceCleanupAfterSuccess::Never
    );

    let invalid = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nworkspace:\n  cleanup:\n    after_success: always\n---\nPrompt\n",
    );
    let error = EffectiveConfig::load(Some(invalid)).unwrap_err();
    match error {
        SymphonyError::ConfigValidation { code, .. } => {
            assert_eq!(code, "invalid_workspace_cleanup_after_success");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn source_id_and_github_repository_list_are_parsed() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let workflow = "---\nsource:\n  id: api\ntracker:\n  kind: github\n  repositories:\n    - owner: octo\n      name: api\n    - owner: octo\n      name: worker\n  project:\n    owner_type: organization\n    owner_login: octo\n    number: 7\n---\nPrompt\n";
    let path = write_workflow(temp.path(), workflow);

    let config = load_from_path(path);
    config.validate_dispatch().unwrap();
    let github = config.tracker.github.as_ref().unwrap();

    assert_eq!(config.source.id, "api");
    assert_eq!(github.repository_owner, "octo");
    assert_eq!(github.repository_name, "api");
    assert_eq!(github.repositories.len(), 2);
    assert_eq!(github.repositories[1].owner, "octo");
    assert_eq!(github.repositories[1].name, "worker");
}

#[test]
fn duplicate_source_ids_are_rejected_for_config_sets() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.md");
    let second = temp.path().join("second.md");
    std::fs::write(&first, valid_workflow(None)).unwrap();
    std::fs::write(&second, valid_workflow(None)).unwrap();

    let error = match ConfigSetReloader::new(vec![first, second]) {
        Ok(_) => panic!("expected duplicate source id error"),
        Err(error) => error,
    };

    match error {
        SymphonyError::ConfigValidation { code, .. } => assert_eq!(code, "duplicate_source_id"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn source_ids_with_colliding_workspace_segments_are_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.md");
    let second = temp.path().join("second.md");
    std::fs::write(
        &first,
        valid_workflow(None).replacen("tracker:", "source:\n  id: api/service\ntracker:", 1),
    )
    .unwrap();
    std::fs::write(
        &second,
        valid_workflow(None).replacen("tracker:", "source:\n  id: api?service\ntracker:", 1),
    )
    .unwrap();

    let error = match ConfigSetReloader::new(vec![first, second]) {
        Ok(_) => panic!("expected colliding workspace source segment error"),
        Err(error) => error,
    };
    match error {
        SymphonyError::ConfigValidation { code, .. } => {
            assert_eq!(code, "colliding_source_workspace_key");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn provided_invalid_hooks_timeout_and_agent_max_turns_never_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();

    for (section, code) in [
        ("hooks:\n  timeout_ms: true\n", "invalid_hooks_timeout_ms"),
        ("hooks:\n  timeout_ms: [1]\n", "invalid_hooks_timeout_ms"),
        ("hooks:\n  timeout_ms: 1.5\n", "invalid_hooks_timeout_ms"),
        (
            "hooks:\n  timeout_ms: \"1.5\"\n",
            "invalid_hooks_timeout_ms",
        ),
        (
            "hooks:\n  timeout_ms: \"18446744073709551616\"\n",
            "invalid_hooks_timeout_ms",
        ),
        ("hooks:\n  timeout_ms: 0\n", "invalid_hooks_timeout_ms"),
        ("agent:\n  max_turns: 1.5\n", "invalid_max_turns"),
        ("hooks:\n  timeout_ms: -1\n", "invalid_hooks_timeout_ms"),
        ("agent:\n  max_turns: true\n", "invalid_max_turns"),
        ("agent:\n  max_turns: [1]\n", "invalid_max_turns"),
        ("agent:\n  max_turns: \"1.5\"\n", "invalid_max_turns"),
        ("agent:\n  max_turns: \"4294967296\"\n", "invalid_max_turns"),
        ("agent:\n  max_turns: 0\n", "invalid_max_turns"),
        ("agent:\n  max_turns: -1\n", "invalid_max_turns"),
    ] {
        let workflow = format!(
            "---\n{section}{}",
            valid_workflow(None).strip_prefix("---\n").unwrap()
        );
        let path = write_workflow(temp.path(), &workflow);
        assert_config_code(path, code);
    }
}

#[test]
fn repository_and_repositories_are_mutually_exclusive() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let workflow = "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  repositories:\n    - owner: octo\n      name: repo\n  project:\n    owner_login: octo\n    number: 7\n---\nPrompt\n";
    let path = write_workflow(temp.path(), workflow);

    let error = EffectiveConfig::load(Some(path)).unwrap_err();

    match error {
        SymphonyError::ConfigValidation { code, .. } => {
            assert_eq!(code, "invalid_github_repository_config");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn github_workflow_endpoint_is_taken_from_front_matter() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let endpoint = "https://github.example/api/graphql";
    let workflow = format!(
        "---\ntracker:\n  kind: github\n  endpoint: {endpoint}\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_type: organization\n    owner_login: octo\n    number: 7\n---\nPrompt\n"
    );
    let path = write_workflow(temp.path(), &workflow);

    let config = load_from_path(path);
    config.validate_dispatch().unwrap();

    assert_eq!(config.tracker.endpoint, endpoint);
}

#[test]
fn path_environment_and_home_expansion_are_applied_only_for_workspace_root() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let env_root = temp.path().join("env-root");
    unsafe { env::set_var("SYMPHONY_TEST_WORKSPACE_ROOT", &env_root) };

    let env_path = write_workflow(
        temp.path(),
        &valid_workflow(Some("$SYMPHONY_TEST_WORKSPACE_ROOT")),
    );
    let env_config = load_from_path(env_path);
    assert_eq!(env_config.workspace.root, env_root);

    let home = tempfile::tempdir().unwrap();
    let previous_home = env::var_os("HOME");
    unsafe { env::set_var("HOME", home.path()) };
    let home_path = write_workflow(
        home.path(),
        &valid_workflow(Some("~/symphony-unit-workspaces")),
    );
    let home_config = load_from_path(home_path);
    match previous_home {
        Some(previous_home) => unsafe { env::set_var("HOME", previous_home) },
        None => unsafe { env::remove_var("HOME") },
    }
    assert_eq!(
        home_config.workspace.root,
        home.path().join("symphony-unit-workspaces")
    );
}

#[test]
fn relative_workspace_root_resolves_against_workflow_directory() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("repo").join("config");
    std::fs::create_dir_all(&nested).unwrap();
    let path = write_workflow(&nested, &valid_workflow(Some("workspaces/../ws")));

    let config = load_from_path(path);
    assert_eq!(config.workspace.root, nested.join("ws"));
}

#[test]
fn invalid_reload_returns_error_and_preserves_last_known_good_config() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), &valid_workflow(None));
    let mut reloader = ConfigReloader::new(Some(path.clone())).unwrap();
    let last_good_prompt = reloader.current().prompt_template.clone();

    std::fs::write(&path, "---\ntracker: [not-a-map]\n---\nBroken").unwrap();
    assert!(reloader.reload_now().is_err());
    assert_eq!(reloader.current().prompt_template, last_good_prompt);
    assert_eq!(reloader.current().tracker.kind, "github");
}

#[test]
fn per_state_concurrency_keys_are_normalized_and_invalid_values_ignored() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nagent:\n  max_concurrent_agents_by_state:\n    Todo: 2\n    In Progress: \"3\"\n    zero: 0\n    negative: -1\n    bad: nope\n---\nPrompt\n",
    );

    let config = load_from_path(path);
    assert_eq!(config.agent.max_concurrent_agents_by_state.len(), 2);
    assert_eq!(config.agent.max_concurrent_agents_by_state["todo"], 2);
    assert_eq!(
        config.agent.max_concurrent_agents_by_state["in progress"],
        3
    );
}

#[test]
fn completion_direct_commit_config_is_parsed_and_validated() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let valid = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\ncompletion:\n  direct_commit:\n    enabled: true\n    base_branch: trunk\n    started_state: In progress\n    high_review_state: In review\n    auto_approved_state: Done\n    commit_author_name: Bot\n    commit_author_email: bot@example.test\n---\nPrompt\n",
    );
    let config = load_from_path(valid);
    assert!(config.completion.direct_commit.enabled);
    assert_eq!(config.completion.direct_commit.base_branch, "trunk");
    assert_eq!(
        config.completion.direct_commit.high_review_state,
        "In review"
    );
    assert_eq!(config.completion.direct_commit.auto_approved_state, "Done");
    assert_eq!(
        config.completion.direct_commit.started_state.as_deref(),
        Some("In progress")
    );
    assert_eq!(config.completion.direct_commit.commit_author_name, "Bot");
    assert_eq!(
        config.completion.direct_commit.commit_author_email,
        "bot@example.test"
    );

    let invalid = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\ncompletion:\n  direct_commit:\n    enabled: true\n    high_review_state: \"  \"\n---\nPrompt\n",
    );
    let error = EffectiveConfig::load(Some(invalid)).unwrap_err();
    match error {
        SymphonyError::ConfigValidation { code, .. } => {
            assert_eq!(code, "invalid_completion_high_review_state");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn server_config_is_parsed_and_validated() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let valid = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nserver:\n  host: 127.0.0.1\n  port: 0\n---\nPrompt\n",
    );
    let config = load_from_path(valid);
    assert_eq!(
        config.server.host,
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
    );
    assert_eq!(config.server.port, Some(0));

    let invalid_host = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nserver:\n  host: not-an-ip\n  port: 8080\n---\nPrompt\n",
    );
    assert_config_code(invalid_host, "invalid_server_host");

    let invalid_negative = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nserver:\n  port: -1\n---\nPrompt\n",
    );
    assert_config_code(invalid_negative, "invalid_server_port");

    let invalid_large = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nserver:\n  port: 65536\n---\nPrompt\n",
    );
    assert_config_code(invalid_large, "invalid_server_port");
}

#[test]
fn conflicting_server_binds_are_rejected_without_cli_override() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.md");
    let second = temp.path().join("second.md");
    std::fs::write(&first, server_workflow("first", 8080)).unwrap();
    std::fs::write(&second, server_workflow("second", 9090)).unwrap();
    let reloader = ConfigSetReloader::new(vec![first, second]).unwrap();

    let error = reloader.initial_server_bind(None, None).unwrap_err();
    match error {
        SymphonyError::ConfigValidation { code, .. } => {
            assert_eq!(code, "conflicting_server_bind");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let bind = reloader
        .initial_server_bind(
            Some("0.0.0.0".parse::<std::net::IpAddr>().unwrap()),
            Some(0),
        )
        .unwrap()
        .unwrap();
    assert_eq!(bind.ip(), "0.0.0.0".parse::<std::net::IpAddr>().unwrap());
    assert_eq!(bind.port(), 0);
}

fn server_workflow(source_id: &str, port: u16) -> String {
    format!(
        "---\nsource:\n  id: {source_id}\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nserver:\n  host: 127.0.0.1\n  port: {port}\n---\nPrompt\n"
    )
}

fn assert_config_code(path: PathBuf, expected: &'static str) {
    let error = EffectiveConfig::load(Some(path)).unwrap_err();
    match error {
        SymphonyError::ConfigValidation { code, .. } => assert_eq!(code, expected),
        other => panic!("unexpected error: {other:?}"),
    }
}
