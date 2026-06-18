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
