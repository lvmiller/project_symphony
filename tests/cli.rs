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
