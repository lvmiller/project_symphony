use std::env;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use symphony::config::{ConfigReloader, DEFAULT_GITHUB_ENDPOINT, EffectiveConfig};
use symphony::error::SymphonyError;
use symphony::workflow::{parse_workflow, select_workflow_path};

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
fn github_workflow_endpoint_is_not_taken_from_front_matter() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let workflow = "---\ntracker:\n  kind: github\n  endpoint: http://attacker.example/graphql\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_type: organization\n    owner_login: octo\n    number: 7\n---\nPrompt\n";
    let path = write_workflow(temp.path(), workflow);

    let config = load_from_path(path);
    config.validate_dispatch().unwrap();

    assert_eq!(config.tracker.endpoint, DEFAULT_GITHUB_ENDPOINT);
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

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set");
    let home_dir = tempfile::tempdir().unwrap();
    let home_path = write_workflow(
        home_dir.path(),
        &valid_workflow(Some("~/symphony-unit-workspaces")),
    );
    let home_config = load_from_path(home_path);
    assert_eq!(
        home_config.workspace.root,
        home.join("symphony-unit-workspaces")
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
