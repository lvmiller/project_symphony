use std::env;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use symphony::config::{
    ConfigDiagnosticStatus, ConfigReloader, ConfigSetReloader, DEFAULT_GITHUB_ENDPOINT,
    EffectiveConfig, EnvironmentValuePresence, TrackerApiKeySource, WorkspaceCleanupAfterSuccess,
    WorkspacePopulationKind, WorkspacePopulationReusePolicy, WorkspaceRootSource,
    config_reload_error_class, raw_workflow_json_schema, workflow_diagnostics,
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
    assert_eq!(config.workspace.retention.max_age_days, None);
    assert!(!config.completion.direct_commit.enabled);
    assert!(!config.completion.direct_commit.dry_run);
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
fn workspace_retention_is_optional_and_requires_positive_age() {
    let temp = tempfile::tempdir().unwrap();
    let enabled = write_workflow(
        temp.path(),
        "---\nworkspace:\n  retention:\n    max_age_days: 14\n---\nPrompt\n",
    );
    assert_eq!(
        load_from_path(enabled).workspace.retention.max_age_days,
        Some(14)
    );

    for workflow in [
        "---\nworkspace:\n  retention:\n    max_age_days: 0\n---\nPrompt\n",
        "---\nworkspace:\n  retention:\n    max_age_days: old\n---\nPrompt\n",
        "---\nworkspace:\n  retention: 14\n---\nPrompt\n",
    ] {
        assert_config_code(
            write_workflow(temp.path(), workflow),
            if workflow.contains("retention: 14") {
                "invalid_workspace_retention"
            } else {
                "invalid_workspace_retention_max_age_days"
            },
        );
    }
}

#[test]
fn workspace_population_is_defaulted_parsed_and_validated() {
    let temp = tempfile::tempdir().unwrap();
    let default_config = load_from_path(write_workflow(temp.path(), &valid_workflow(None)));
    assert_eq!(
        default_config.workspace.population.kind,
        WorkspacePopulationKind::None
    );
    assert_eq!(
        default_config.workspace.population.reuse,
        WorkspacePopulationReusePolicy::Skip
    );

    let git = write_workflow(
        temp.path(),
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nworkspace:\n  population:\n    kind: git\n    repository_url: https://github.com/octo/repo.git\n    ref: refs/tags/v1\n    depth: 1\n    reuse: fetch_ff_only\n---\nPrompt\n",
    );
    let config = load_from_path(git);
    assert_eq!(
        config.workspace.population.kind,
        WorkspacePopulationKind::Git
    );
    assert_eq!(
        config.workspace.population.repository_url.as_deref(),
        Some("https://github.com/octo/repo.git")
    );
    assert_eq!(
        config.workspace.population.reference.as_deref(),
        Some("refs/tags/v1")
    );
    assert_eq!(config.workspace.population.depth, Some(1));
    assert_eq!(
        config.workspace.population.reuse,
        WorkspacePopulationReusePolicy::FetchFfOnly
    );

    let missing_url = write_workflow(
        temp.path(),
        "---\nworkspace:\n  population:\n    kind: git\n---\nPrompt\n",
    );
    assert_config_code(missing_url, "missing_workspace_population_repository_url");
    let conflicting_target = write_workflow(
        temp.path(),
        "---\nworkspace:\n  population:\n    kind: git\n    repository_url: https://github.com/octo/repo.git\n    ref: refs/heads/main\n    branch: main\n---\nPrompt\n",
    );
    assert_config_code(conflicting_target, "invalid_workspace_population_reference");
    let option_like_branch = write_workflow(
        temp.path(),
        "---\nworkspace:\n  population:\n    kind: git\n    repository_url: https://github.com/octo/repo.git\n    branch: --upload-pack=malicious\n---\nPrompt\n",
    );
    assert_config_code(option_like_branch, "invalid_workspace_population_reference");
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
fn case_and_unicode_variant_source_ids_have_distinct_workspace_namespaces() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.md");
    let second = temp.path().join("second.md");
    let third = temp.path().join("third.md");
    let fourth = temp.path().join("fourth.md");
    std::fs::write(
        &first,
        valid_workflow(None).replacen("tracker:", "source:\n  id: Team\ntracker:", 1),
    )
    .unwrap();
    std::fs::write(
        &second,
        valid_workflow(None).replacen("tracker:", "source:\n  id: team\ntracker:", 1),
    )
    .unwrap();
    std::fs::write(
        &third,
        valid_workflow(None).replacen("tracker:", "source:\n  id: \"é\"\ntracker:", 1),
    )
    .unwrap();
    std::fs::write(
        &fourth,
        valid_workflow(None).replacen("tracker:", "source:\n  id: \"e\u{301}\"\ntracker:", 1),
    )
    .unwrap();

    let reloaders = ConfigSetReloader::new(vec![first, second, third, fourth]).unwrap();
    assert_eq!(reloaders.current().count(), 4);
}

#[test]
fn worker_ssh_configuration_defaults_parses_and_rejects_invalid_values() {
    let temp = tempfile::tempdir().unwrap();
    let default = load_from_path(write_workflow(temp.path(), &valid_workflow(None)));
    assert!(default.worker.ssh_hosts.is_empty());
    assert_eq!(default.worker.max_concurrent_agents_per_host, 1);

    let configured = load_from_path(write_workflow(
        temp.path(),
        &valid_workflow(None).replacen(
            "---\ntracker:",
            "---\nworker:\n  ssh_hosts: [build-a, build-b]\n  max_concurrent_agents_per_host: 2\nworkspace:\n  root: /srv/symphony\ntracker:",
            1,
        ),
    ));
    assert_eq!(configured.worker.ssh_hosts, ["build-a", "build-b"]);
    assert_eq!(configured.worker.max_concurrent_agents_per_host, 2);

    for (worker, code) in [
        (
            "worker:\n  ssh_hosts: [Team, team]\n",
            "duplicate_worker_ssh_host",
        ),
        (
            "worker:\n  ssh_hosts: [\"   \"]\n",
            "invalid_worker_ssh_hosts",
        ),
        (
            "worker:\n  ssh_hosts: not-a-list\n",
            "invalid_worker_ssh_hosts",
        ),
        (
            "worker:\n  max_concurrent_agents_per_host: 0\n",
            "invalid_worker_max_concurrent_agents_per_host",
        ),
    ] {
        assert_config_code(
            write_workflow(
                temp.path(),
                &valid_workflow(None).replacen(
                    "---\ntracker:",
                    &format!("---\n{worker}tracker:"),
                    1,
                ),
            ),
            code,
        );
    }

    assert_config_code(
        write_workflow(
            temp.path(),
            &valid_workflow(Some("relative-workspaces")).replacen(
                "---\ntracker:",
                "---\nworker:\n  ssh_hosts: [build-a]\ntracker:",
                1,
            ),
        ),
        "invalid_remote_workspace_root",
    );
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
        ("agent:\n  max_turns: 4294967296\n", "invalid_max_turns"),
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
fn agent_max_turns_accepts_the_u32_limit_without_truncation() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(
        temp.path(),
        &format!(
            "---\nagent:\n  max_turns: {}\n{}",
            u32::MAX,
            valid_workflow(None).strip_prefix("---\n").unwrap()
        ),
    );

    let config = load_from_path(path);

    assert_eq!(config.agent.max_turns, u32::MAX);
}

#[test]
fn workflow_diagnostics_reports_environment_prerequisites_without_secret_values() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), &valid_workflow(None));

    unsafe { env::remove_var("GITHUB_TOKEN") };
    let missing = workflow_diagnostics(Some(path.clone())).unwrap();
    assert_eq!(missing.parse.status, ConfigDiagnosticStatus::Passed);
    assert_eq!(missing.dispatch.status, ConfigDiagnosticStatus::Failed);
    assert!(matches!(
        &missing.tracker_api_key.source,
        TrackerApiKeySource::Environment { variable } if variable == "GITHUB_TOKEN"
    ));
    assert_eq!(
        missing.tracker_api_key.presence,
        EnvironmentValuePresence::Missing
    );
    assert!(!missing.is_healthy());

    unsafe { env::set_var("GITHUB_TOKEN", "") };
    let empty = workflow_diagnostics(Some(path.clone())).unwrap();
    assert_eq!(
        empty.tracker_api_key.presence,
        EnvironmentValuePresence::Empty
    );
    assert_eq!(empty.dispatch.status, ConfigDiagnosticStatus::Failed);

    unsafe { env::set_var("GITHUB_TOKEN", "super-secret-token") };
    let present = workflow_diagnostics(Some(path.clone())).unwrap();
    assert!(present.is_healthy());
    assert_eq!(
        present.tracker_api_key.presence,
        EnvironmentValuePresence::Present
    );
    assert!(
        !serde_json::to_string(&present)
            .unwrap()
            .contains("super-secret-token"),
        "workflow diagnostics must not serialize resolved tracker secrets"
    );

    unsafe { env::set_var("SYMPHONY_TEST_TOKEN", "explicit-secret") };
    let explicit_path = write_workflow(
        temp.path(),
        &valid_workflow(None).replacen(
            "kind: github\n",
            "kind: github\n  api_key: $SYMPHONY_TEST_TOKEN\n",
            1,
        ),
    );
    let explicit = workflow_diagnostics(Some(explicit_path)).unwrap();
    assert!(explicit.is_healthy());
    assert!(matches!(
        &explicit.tracker_api_key.source,
        TrackerApiKeySource::Environment { variable } if variable == "SYMPHONY_TEST_TOKEN"
    ));
    assert!(
        !serde_json::to_string(&explicit)
            .unwrap()
            .contains("explicit-secret")
    );

    let literal_path = write_workflow(
        temp.path(),
        &valid_workflow(None).replacen(
            "kind: github\n",
            "kind: github\n  api_key: configured-literal\n",
            1,
        ),
    );
    let literal = workflow_diagnostics(Some(literal_path)).unwrap();
    assert!(literal.is_healthy());
    assert!(matches!(
        &literal.tracker_api_key.source,
        TrackerApiKeySource::Literal
    ));
    assert_eq!(
        literal.tracker_api_key.presence,
        EnvironmentValuePresence::Present
    );
    assert!(
        !serde_json::to_string(&literal)
            .unwrap()
            .contains("configured-literal")
    );

    let workspace_path = write_workflow(
        temp.path(),
        &valid_workflow(None).replacen(
            "number: 7\n",
            "number: 7\nworkspace:\n  root: $SYMPHONY_TEST_WORKSPACE_ROOT\n",
            1,
        ),
    );
    unsafe { env::remove_var("SYMPHONY_TEST_WORKSPACE_ROOT") };
    let workspace_missing = workflow_diagnostics(Some(workspace_path.clone())).unwrap();
    assert_eq!(
        workspace_missing.parse.status,
        ConfigDiagnosticStatus::Failed
    );
    assert!(matches!(
        &workspace_missing.workspace_root.source,
        WorkspaceRootSource::Environment { variable }
            if variable == "SYMPHONY_TEST_WORKSPACE_ROOT"
    ));
    assert_eq!(
        workspace_missing.workspace_root.environment_presence,
        Some(EnvironmentValuePresence::Missing)
    );
    assert_eq!(
        workspace_missing
            .workspace_root
            .status
            .error_class
            .as_deref(),
        Some("missing_path_env")
    );
    assert_eq!(workspace_missing.workspace_root.normalized_path, None);

    unsafe {
        env::set_var(
            "SYMPHONY_TEST_WORKSPACE_ROOT",
            temp.path().join("nested/../workspace"),
        )
    };
    let workspace_present = workflow_diagnostics(Some(workspace_path)).unwrap();
    assert!(workspace_present.is_healthy());
    assert!(matches!(
        &workspace_present.workspace_root.source,
        WorkspaceRootSource::Environment { variable }
            if variable == "SYMPHONY_TEST_WORKSPACE_ROOT"
    ));
    assert!(workspace_present.workspace_root.normalized_path.is_some());
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
fn github_endpoint_requires_https_except_for_explicit_numeric_loopback() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();

    for endpoint in [
        "http://github.example/graphql",
        "http://localhost/graphql",
        "http://192.168.1.10/graphql",
        "http://[::ffff:127.0.0.1]/graphql",
    ] {
        let workflow = valid_workflow(None).replacen(
            "kind: github",
            &format!("kind: github\n  endpoint: {endpoint}"),
            1,
        );
        assert_config_code(
            write_workflow(temp.path(), &workflow),
            "insecure_tracker_endpoint",
        );
    }

    let loopback = valid_workflow(None).replacen(
        "kind: github",
        "kind: github\n  endpoint: http://127.0.0.1:8080/graphql\n  allow_insecure_loopback: true",
        1,
    );
    let config = load_from_path(write_workflow(temp.path(), &loopback));
    assert!(config.tracker.allow_insecure_loopback);

    let loopback_v6 = valid_workflow(None).replacen(
        "kind: github",
        "kind: github\n  endpoint: http://[::1]:8080/graphql\n  allow_insecure_loopback: true",
        1,
    );
    load_from_path(write_workflow(temp.path(), &loopback_v6));

    let disabled = valid_workflow(None).replacen(
        "kind: github",
        "kind: github\n  endpoint: http://127.0.0.1:8080/graphql",
        1,
    );
    assert_config_code(
        write_workflow(temp.path(), &disabled),
        "insecure_tracker_endpoint",
    );
}

#[test]
fn malformed_tracker_endpoints_do_not_disclose_credentials() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { env::set_var("GITHUB_TOKEN", "unit-token") };
    let temp = tempfile::tempdir().unwrap();
    let secret = "endpoint-token-must-not-leak";
    let workflow = valid_workflow(None).replacen(
        "kind: github",
        &format!("kind: github\n  endpoint: https://{secret}@github.example/graphql"),
        1,
    );
    let error = EffectiveConfig::load(Some(write_workflow(temp.path(), &workflow))).unwrap_err();
    assert!(!error.to_string().contains(secret));
    assert!(matches!(
        error,
        SymphonyError::ConfigValidation {
            code: "invalid_tracker_endpoint",
            ..
        }
    ));
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
    let error = reloader.reload_now().unwrap_err();
    assert_eq!(
        config_reload_error_class(&error),
        "unsupported_tracker_kind"
    );
    assert_eq!(reloader.workflow_path(), path);
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
        "---\ntracker:\n  kind: github\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\ncompletion:\n  direct_commit:\n    enabled: true\n    dry_run: true\n    base_branch: trunk\n    started_state: In progress\n    high_review_state: In review\n    auto_approved_state: Done\n    commit_author_name: Bot\n    commit_author_email: bot@example.test\n---\nPrompt\n",
    );
    let config = load_from_path(valid);
    assert!(config.completion.direct_commit.enabled);
    assert!(config.completion.direct_commit.dry_run);
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
fn raw_workflow_schema_is_static_extensible_and_documents_defaults() {
    let schema = raw_workflow_json_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["additionalProperties"], true);
    assert_eq!(
        schema["properties"]["polling"]["properties"]["interval_ms"]["default"],
        30_000
    );
    assert_eq!(
        schema["properties"]["agent"]["properties"]["max_turns"]["default"],
        20
    );
    assert_eq!(
        schema["properties"]["completion"]["properties"]["direct_commit"]["properties"]["dry_run"]
            ["default"],
        false
    );
    assert_eq!(
        schema["properties"]["workspace"]["properties"]["population"]["properties"]["kind"]["default"],
        "none"
    );
    assert_eq!(
        schema["properties"]["workspace"]["properties"]["retention"]["properties"]["max_age_days"]
            ["minimum"],
        1
    );
    assert!(
        schema["properties"]["workspace"]["properties"]["retention"]["properties"]["max_age_days"]
            .get("default")
            .is_none()
    );
    assert!(
        schema["properties"]["tracker"]["properties"]["api_key"]
            .get("default")
            .is_none()
    );
    assert!(
        schema["properties"]["tracker"]["properties"]["api_key"]["description"]
            .as_str()
            .unwrap()
            .contains("$VAR_NAME")
    );
    assert_eq!(
        schema["properties"]["tracker"]["properties"]["allow_insecure_loopback"]["default"],
        false
    );
    assert_eq!(
        schema["properties"]["server"]["properties"]["refresh_cooldown_ms"]["default"],
        1_000
    );
    assert_eq!(
        schema["properties"]["server"]["properties"]["drain_timeout_ms"]["default"],
        30_000
    );
    assert_eq!(
        schema["properties"]["server"]["properties"]["auth_token"]["writeOnly"],
        true
    );
    assert!(
        schema["properties"]["server"]["properties"]["auth_token"]
            .get("default")
            .is_none()
    );

    let temp = tempfile::tempdir().unwrap();
    let config = load_from_path(write_workflow(
        temp.path(),
        "---\nfuture_extension:\n  feature: enabled\n---\nPrompt\n",
    ));
    assert_eq!(config.polling.interval_ms, 30_000);
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
    assert_eq!(config.server.refresh_cooldown_ms, 1_000);
    assert_eq!(config.server.drain_timeout_ms, 30_000);
    assert!(config.server.auth_token.is_none());

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
fn server_auth_token_is_resolved_redacted_and_required_off_loopback() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        env::set_var("GITHUB_TOKEN", "unit-token");
        env::set_var(
            "SYMPHONY_SERVER_TEST_TOKEN",
            "server-token-must-not-serialize",
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let configured = valid_workflow(None).replacen(
        "---\n",
        "---\nserver:\n  host: 0.0.0.0\n  port: 8080\n  auth_token: $SYMPHONY_SERVER_TEST_TOKEN\n  refresh_cooldown_ms: 250\n  drain_timeout_ms: 500\n",
        1,
    );
    let config = load_from_path(write_workflow(temp.path(), &configured));
    assert_eq!(
        config.server.auth_token.as_deref(),
        Some("server-token-must-not-serialize")
    );
    assert_eq!(config.server.refresh_cooldown_ms, 250);
    assert_eq!(config.server.drain_timeout_ms, 500);
    assert!(
        !serde_json::to_string(&config)
            .unwrap()
            .contains("server-token-must-not-serialize")
    );
    assert!(!format!("{config:?}").contains("server-token-must-not-serialize"));

    let missing_auth = valid_workflow(None).replacen(
        "---\n",
        "---\nserver:\n  host: 192.168.1.9\n  port: 8080\n",
        1,
    );
    assert_config_code(
        write_workflow(temp.path(), &missing_auth),
        "missing_server_auth_token",
    );
    let zero_cooldown =
        valid_workflow(None).replacen("---\n", "---\nserver:\n  refresh_cooldown_ms: 0\n", 1);
    assert_config_code(
        write_workflow(temp.path(), &zero_cooldown),
        "invalid_server_refresh_cooldown_ms",
    );
    let zero_drain_timeout =
        valid_workflow(None).replacen("---\n", "---\nserver:\n  drain_timeout_ms: 0\n", 1);
    assert_config_code(
        write_workflow(temp.path(), &zero_drain_timeout),
        "invalid_server_drain_timeout_ms",
    );
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
