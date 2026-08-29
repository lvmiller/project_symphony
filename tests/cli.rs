use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn cli_fails_cleanly_when_default_workflow_is_missing() {
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .arg("--check")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("startup_failed")
                .and(predicate::str::contains("missing_workflow_file"))
                .and(predicate::str::contains("WORKFLOW.md")),
        );
}

#[test]
fn cli_fails_cleanly_when_default_workflow_path_is_directory() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join("WORKFLOW.md")).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .arg("--check")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("startup_failed")
                .and(predicate::str::contains("workflow_path_not_file"))
                .and(predicate::str::contains("WORKFLOW.md")),
        );
}

#[test]
fn cli_uses_default_workflow_path_for_check_success() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("WORKFLOW.md"), valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .arg("--check")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("startup completed"));
}

#[test]
fn cli_fails_cleanly_when_explicit_workflow_is_missing() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing.md");

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .arg(&missing)
        .arg("--check")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("startup_failed")
                .and(predicate::str::contains("missing_workflow_file"))
                .and(predicate::str::contains("missing.md")),
        );
}

#[test]
fn cli_uses_explicit_workflow_path_for_check_success() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("custom.md");
    std::fs::write(&workflow, valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .arg(&workflow)
        .arg("--check")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("startup completed"));
}

#[test]
fn cli_accepts_multiple_workflow_paths_for_check_success() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("api.md");
    let second = temp.path().join("worker.md");
    std::fs::write(&first, valid_source_workflow("api")).unwrap();
    std::fs::write(&second, valid_source_workflow("worker")).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .arg(&first)
        .arg(&second)
        .arg("--check")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("startup completed")
                .and(predicate::str::contains("api:"))
                .and(predicate::str::contains("worker:")),
        );
}

#[test]
fn cli_accepts_port_in_check_mode_without_binding() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("custom.md");
    std::fs::write(&workflow, valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .arg("--check")
        .arg("--port")
        .arg("0")
        .arg(&workflow)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("check completed"));
}

#[test]
fn cli_rejects_invalid_host() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("WORKFLOW.md"), valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .arg("--host")
        .arg("not-an-ip")
        .arg("--check")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("not-an-ip"));
}

#[test]
fn config_validate_uses_default_workflow_path() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("WORKFLOW.md"), valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"));
}

#[test]
fn config_validate_uses_explicit_workflow_path() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("custom.md");
    std::fs::write(&workflow, valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "test-token")
        .args(["config", "validate"])
        .arg(&workflow)
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"));
}

#[test]
fn config_validate_reports_missing_default_workflow() {
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .args(["config", "validate"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("missing_workflow_file")
                .and(predicate::str::contains("WORKFLOW.md")),
        );
}

#[test]
fn config_validate_reports_missing_explicit_workflow() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing.md");

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .args(["config", "validate"])
        .arg(&missing)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("missing_workflow_file")
                .and(predicate::str::contains("missing.md")),
        );
}

#[test]
fn config_validate_reports_missing_tracker_token() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("WORKFLOW.md"), valid_workflow()).unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("GITHUB_TOKEN")
        .args(["config", "validate"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("missing_tracker_api_key"));
}

#[test]
fn config_explain_json_shows_defaults_without_exposing_token() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("custom.md");
    let token = "super-secret-token";
    std::fs::write(&workflow, workflow_with_defaults()).unwrap();

    let output = Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", token)
        .args(["config", "explain"])
        .arg(&workflow)
        .args(["--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains(token));
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["tracker"]["api_key_present"], true);
    assert!(report["tracker"].get("api_key").is_none());
    assert_eq!(report["polling"]["interval_ms"], 30_000);
    assert_eq!(report["agent"]["max_turns"], 20);
    assert_eq!(
        report["workspace"]["root"],
        temp.path().join("workspaces").to_string_lossy().as_ref()
    );
}

#[test]
fn config_schema_emits_json_without_workflow_or_token() {
    let temp = TempDir::new().unwrap();

    let output = Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("GITHUB_TOKEN")
        .args(["config", "schema"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let schema: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(schema["properties"]["tracker"].is_object());
    assert_eq!(
        schema["properties"]["polling"]["properties"]["interval_ms"]["default"],
        30_000
    );
    assert_eq!(schema["additionalProperties"], true);
    assert_eq!(
        schema["properties"]["agent"]["properties"]["max_turns"]["default"],
        20
    );
}

#[test]
fn config_doctor_reports_literal_tracker_key_without_exposing_it() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("literal.md");
    let sentinel = "super-secret-token";
    std::fs::write(
        &workflow,
        workflow_with_tracker_key(sentinel, "workspace:\n  root: workspaces"),
    )
    .unwrap();

    let output = Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "another-secret-token")
        .args(["config", "doctor"])
        .arg(&workflow)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("workflow_path"));
    assert!(stdout.contains("parse_status"));
    assert!(stdout.contains("dispatch_validation_status"));
    assert!(stdout.contains("status: passed"));
    assert!(stdout.contains("source: literal"));
    assert!(stdout.contains("presence: present"));
    assert!(!stdout.contains(sentinel));
    assert!(!stderr.contains(sentinel));
}

#[test]
fn config_doctor_reports_github_token_indirection_presence() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("github-token.md");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key("$GITHUB_TOKEN", "workspace:\n  root: workspaces"),
    )
    .unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "super-secret-token")
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("source: $GITHUB_TOKEN")
                .and(predicate::str::contains("presence: present"))
                .and(predicate::str::contains("super-secret-token").not()),
        )
        .stderr(predicate::str::contains("super-secret-token").not());
}

#[test]
fn config_doctor_reports_custom_token_indirection_presence() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("custom-token.md");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key("$SYMPHONY_DOCTOR_TOKEN", "workspace:\n  root: workspaces"),
    )
    .unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("SYMPHONY_DOCTOR_TOKEN", "super-secret-token")
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("source: $SYMPHONY_DOCTOR_TOKEN")
                .and(predicate::str::contains("presence: present"))
                .and(predicate::str::contains("super-secret-token").not()),
        )
        .stderr(predicate::str::contains("super-secret-token").not());
}

#[test]
fn config_doctor_fails_for_missing_required_token() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("missing-token.md");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key("$GITHUB_TOKEN", "workspace:\n  root: workspaces"),
    )
    .unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("GITHUB_TOKEN")
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("source: $GITHUB_TOKEN")
                .and(predicate::str::contains("presence: missing")),
        )
        .stderr(predicate::str::contains("config_failed"));
}

#[test]
fn config_doctor_fails_for_empty_required_token() {
    let temp = TempDir::new().unwrap();

    let workflow = temp.path().join("empty-token.md");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key("$GITHUB_TOKEN", "workspace:\n  root: workspaces"),
    )
    .unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "")
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("source: $GITHUB_TOKEN")
                .and(predicate::str::contains("presence: empty")),
        )
        .stderr(predicate::str::contains("config_failed"));
}

#[test]
fn config_doctor_fails_for_workflow_parse_errors() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("invalid.md");
    std::fs::write(&workflow, "---\ntracker: [\n---\n").unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("parse_status")
                .and(predicate::str::contains("status: failed"))
                .and(predicate::str::contains("workflow_parse_error")),
        )
        .stderr(predicate::str::contains("config_failed"));
}

#[test]
fn config_doctor_reports_normalized_workspace_root_from_environment() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("root-env.md");
    let root = temp
        .path()
        .join("nested")
        .join("..")
        .join("diagnostic-root");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key(
            "$GITHUB_TOKEN",
            "workspace:\n  root: $SYMPHONY_WORKSPACE_ROOT",
        ),
    )
    .unwrap();

    let output = Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "super-secret-token")
        .env("SYMPHONY_WORKSPACE_ROOT", &root)
        .args(["config", "doctor"])
        .arg(&workflow)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("source: $SYMPHONY_WORKSPACE_ROOT"));
    assert!(stdout.contains("environment: SYMPHONY_WORKSPACE_ROOT"));
    assert!(stdout.contains("presence: present"));
    assert!(stdout.contains("normalized_path"));
    assert!(
        stdout.contains(
            temp.path()
                .join("diagnostic-root")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(!stdout.contains("super-secret-token"));
}

#[test]
fn config_doctor_fails_for_missing_workspace_root_environment() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("missing-root-env.md");
    std::fs::write(
        &workflow,
        workflow_with_tracker_key(
            "$GITHUB_TOKEN",
            "workspace:\n  root: $SYMPHONY_WORKSPACE_ROOT",
        ),
    )
    .unwrap();

    Command::cargo_bin("symphony")
        .unwrap()
        .current_dir(temp.path())
        .env("GITHUB_TOKEN", "super-secret-token")
        .env_remove("SYMPHONY_WORKSPACE_ROOT")
        .args(["config", "doctor"])
        .arg(&workflow)
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("source: $SYMPHONY_WORKSPACE_ROOT")
                .and(predicate::str::contains(
                    "environment: SYMPHONY_WORKSPACE_ROOT",
                ))
                .and(predicate::str::contains("presence: missing")),
        )
        .stderr(
            predicate::str::contains("config_failed")
                .and(predicate::str::contains("super-secret-token").not()),
        );
}

#[test]
fn config_commands_respect_explicit_environment_semantics_and_redact_secrets() {
    let temp = TempDir::new().unwrap();
    let workflow = temp.path().join("literal-key.md");
    let sentinel = "super-secret-token";
    std::fs::write(
        &workflow,
        workflow_with_tracker_key(sentinel, "workspace:\n  root: workspaces"),
    )
    .unwrap();

    for command in [
        vec!["config", "doctor"],
        vec!["config", "explain", "--format", "json"],
        vec!["config", "validate"],
    ] {
        let output = Command::cargo_bin("symphony")
            .unwrap()
            .current_dir(temp.path())
            .env("GITHUB_TOKEN", "environment-secret-token")
            .args(command)
            .arg(&workflow)
            .output()
            .unwrap();

        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8(output.stdout).unwrap().contains(sentinel));
        assert!(!String::from_utf8(output.stderr).unwrap().contains(sentinel));
    }
}

fn workflow_with_defaults() -> &'static str {
    r#"---
tracker:
  kind: github
  repository:
    owner: acme
    name: symphony
  project:
    owner_type: organization
    owner_login: acme
    number: 1
    status_field: Status
workspace:
  root: workspaces
---
Handle {{ issue.identifier }}
"#
}

fn workflow_with_tracker_key(api_key: &str, workspace: &str) -> String {
    format!(
        r#"---
tracker:
  kind: github
  api_key: {api_key}
  repository:
    owner: acme
    name: symphony
  project:
    owner_type: organization
    owner_login: acme
    number: 1
    status_field: Status
{workspace}
agent:
  max_turns: 1
---
Handle {{{{ issue.identifier }}}}
"#
    )
}

fn valid_workflow() -> &'static str {
    r#"---
tracker:
  kind: github
  repository:
    owner: acme
    name: symphony
  project:
    owner_type: organization
    owner_login: acme
    number: 1
    status_field: Status
agent:
  max_turns: 1
---
Handle {{ issue.identifier }}
"#
}

fn valid_source_workflow(source_id: &str) -> String {
    format!(
        r#"---
source:
  id: {source_id}
tracker:
  kind: github
  repository:
    owner: acme
    name: symphony
  project:
    owner_type: organization
    owner_login: acme
    number: 1
agent:
  max_turns: 1
---
Handle {{{{ issue.identifier }}}}
"#
    )
}
