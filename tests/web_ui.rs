use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use reqwest::StatusCode;
use serde_json::{Value, json};
use symphony::config::EffectiveConfig;
use symphony::observability::http::{SharedStatus, spawn_http_server};
use symphony::orchestrator::OrchestratorState;
use tokio::sync::mpsc;

fn workflow() -> &'static str {
    "---\ntracker:\n  kind: github\n  api_key: super-secret\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\n---\nPrompt\n"
}

fn write_workflow(dir: &Path) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    fs::write(&path, workflow()).unwrap();
    path
}

async fn start_server(
    path: &Path,
) -> (
    String,
    symphony::observability::http::HttpServerHandle,
    mpsc::UnboundedReceiver<()>,
) {
    let config = EffectiveConfig::load(Some(path.to_path_buf())).unwrap();
    let shared_status = SharedStatus::new(std::slice::from_ref(&config));
    shared_status
        .publish(&OrchestratorState::default(), &[config])
        .await;
    let (refresh_tx, refresh_rx) = mpsc::unbounded_channel();
    let server = spawn_http_server(
        "127.0.0.1:0".parse().unwrap(),
        shared_status,
        refresh_tx,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    let base = format!("http://{}", server.local_addr);
    (base, server, refresh_rx)
}

#[tokio::test]
async fn state_api_returns_counts_and_omits_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path).await;

    let text = reqwest::get(format!("{base}/api/v1/state"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(json["counts"]["sources"], 1);
    assert_eq!(json["counts"]["running"], 0);
    assert!(!text.contains("super-secret"));
    server.task.abort();
}

#[tokio::test]
async fn dashboard_html_contains_expected_markers() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path).await;

    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(html.contains("Symphony"));
    assert!(html.contains("Repository"));
    assert!(html.contains("/api/v1/state"));
    server.task.abort();
}

#[tokio::test]
async fn repository_add_http_flow_updates_workflow_and_get_response() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path).await;
    let client = reqwest::Client::new();

    let added: Value = client
        .post(format!("{base}/api/v1/repositories"))
        .json(&json!({"source_id":"default","owner":"octo","name":"worker"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(added["repositories"].as_array().unwrap().len(), 2);
    let listed: Value = client
        .get(format!("{base}/api/v1/repositories?source_id=default"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(listed["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(listed["repositories"][1]["name"], "worker");
    assert!(fs::read_to_string(&path).unwrap().contains("repositories:"));
    server.task.abort();
}

#[tokio::test]
async fn delete_last_repository_returns_conflict_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let before = fs::read(&path).unwrap();
    let (base, server, _refresh_rx) = start_server(&path).await;
    let client = reqwest::Client::new();

    let response = client
        .delete(format!(
            "{base}/api/v1/repositories?source_id=default&owner=octo&name=repo"
        ))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "last_repository");
    assert_eq!(fs::read(&path).unwrap(), before);
    server.task.abort();
}

#[tokio::test]
async fn refresh_route_coalesces_pending_requests() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, mut refresh_rx) = start_server(&path).await;
    let client = reqwest::Client::new();

    let first: Value = client
        .post(format!("{base}/api/v1/refresh"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["queued"], true);
    assert_eq!(first["coalesced"], false);
    refresh_rx.recv().await.unwrap();

    let second: Value = client
        .post(format!("{base}/api/v1/refresh"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["queued"], true);
    assert_eq!(second["coalesced"], true);
    assert!(refresh_rx.try_recv().is_err());
    server.task.abort();
}

#[tokio::test]
async fn unknown_issue_identifier_returns_not_found_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path).await;
    let encoded = "octo%2Frepo%23123";

    let response = reqwest::get(format!("{base}/api/v1/{encoded}"))
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "issue_not_found");
    server.task.abort();
}
