use std::fs;
use std::path::{Path, PathBuf};

use symphony::error::SymphonyError;
use symphony::workflow_store::{RepositoryMutation, add_repository, remove_repository};

fn write_workflow(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    fs::write(&path, contents).unwrap();
    path
}

fn single_repo_workflow() -> &'static str {
    "---\ntracker:\n  kind: github\n  api_key: test-token\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\n---\nPrompt body\n  keep me\n"
}

fn multi_repo_workflow() -> &'static str {
    "---\ntracker:\n  kind: github\n  api_key: test-token\n  repositories:\n    - owner: octo\n      name: repo\n    - owner: octo\n      name: worker\n  project:\n    owner_login: octo\n    number: 7\n---\nPrompt body\n"
}

#[test]
fn add_repository_converts_single_repo_and_preserves_prompt_body() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), single_repo_workflow());

    let list = add_repository(
        &path,
        RepositoryMutation {
            owner: "octo".to_string(),
            name: "worker".to_string(),
        },
    )
    .unwrap();
    let rewritten = fs::read_to_string(&path).unwrap();

    assert_eq!(list.repositories.len(), 2);
    assert_eq!(list.repositories[0].name, "repo");
    assert_eq!(list.repositories[1].name, "worker");
    assert!(rewritten.contains("repositories:"));
    assert!(rewritten.contains("name: repo"));
    assert!(rewritten.contains("name: worker"));
    assert!(!rewritten.contains("repository:\n"));
    assert!(rewritten.ends_with("---\nPrompt body\n  keep me\n"));
}

#[test]
fn duplicate_add_is_case_insensitive_and_leaves_file_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), multi_repo_workflow());
    let before = fs::read(&path).unwrap();

    let error = add_repository(
        &path,
        RepositoryMutation {
            owner: "OCTO".to_string(),
            name: "REPO".to_string(),
        },
    )
    .unwrap_err();

    assert_error_code(error, "duplicate_repository");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn remove_repository_from_multi_repo_workflow_leaves_one_entry() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), multi_repo_workflow());

    let list = remove_repository(
        &path,
        RepositoryMutation {
            owner: "octo".to_string(),
            name: "worker".to_string(),
        },
    )
    .unwrap();
    let rewritten = fs::read_to_string(&path).unwrap();

    assert_eq!(list.repositories.len(), 1);
    assert_eq!(list.repositories[0].name, "repo");
    assert!(rewritten.contains("repositories:"));
    assert!(rewritten.contains("name: repo"));
    assert!(!rewritten.contains("worker"));
}

#[test]
fn removing_last_repository_leaves_file_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), single_repo_workflow());
    let before = fs::read(&path).unwrap();

    let error = remove_repository(
        &path,
        RepositoryMutation {
            owner: "octo".to_string(),
            name: "repo".to_string(),
        },
    )
    .unwrap_err();

    assert_error_code(error, "last_repository");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn missing_owner_or_name_is_rejected_and_leaves_file_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path(), single_repo_workflow());
    let before = fs::read(&path).unwrap();

    let error = add_repository(
        &path,
        RepositoryMutation {
            owner: " ".to_string(),
            name: "worker".to_string(),
        },
    )
    .unwrap_err();

    assert_error_code(error, "invalid_repository");
    assert_eq!(fs::read(&path).unwrap(), before);
}

fn assert_error_code(error: SymphonyError, expected: &'static str) {
    match error {
        SymphonyError::ConfigValidation { code, .. } => assert_eq!(code, expected),
        other => panic!("unexpected error: {other:?}"),
    }
}
