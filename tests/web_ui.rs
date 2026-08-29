use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::{TimeZone, Utc};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use symphony::config::EffectiveConfig;
use symphony::domain::{CodexEvent, Issue, RetryEntry, TokenTotals};
use symphony::observability::http::{SharedStatus, spawn_http_server};
use symphony::orchestrator::OrchestratorState;
use symphony::time::ms_from_now;
use tokio::sync::mpsc;

fn workflow() -> &'static str {
    "---\ntracker:\n  kind: github\n  api_key: super-secret\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\n---\nPrompt\n"
}

fn write_workflow(dir: &Path) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    fs::write(&path, workflow()).unwrap();
    path
}

fn issue(id: &str, identifier: &str, state: &str) -> Issue {
    Issue {
        id: id.to_string(),
        identifier: identifier.to_string(),
        title: format!("Issue {identifier}"),
        description: None,
        priority: None,
        state: state.to_string(),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

fn populated_state() -> OrchestratorState {
    let started_at = Utc
        .with_ymd_and_hms(2026, 2, 24, 20, 10, 12)
        .single()
        .unwrap();
    let event_at = Utc
        .with_ymd_and_hms(2026, 2, 24, 20, 14, 59)
        .single()
        .unwrap();
    let mut state = OrchestratorState::default();
    state.claim_running(
        issue("abc123", "MT-649", "In Progress"),
        Some(2),
        started_at,
    );
    state.apply_codex_event(CodexEvent {
        issue_id: "abc123".to_string(),
        event: "notification".to_string(),
        timestamp: event_at,
        session_id: Some("thread-1-turn-1".to_string()),
        thread_id: Some("thread-1".to_string()),
        turn_id: Some("turn-1".to_string()),
        codex_app_server_pid: Some(42),
        message: Some("Working on tests".to_string()),
        absolute_token_totals: Some(TokenTotals {
            input_tokens: 1200,
            output_tokens: 800,
            total_tokens: 2000,
        }),
        rate_limits: Some(json!({"remaining": 12})),
    });
    state.retry_attempts.insert(
        "def456".to_string(),
        RetryEntry {
            source_id: "default".to_string(),
            issue_id: "def456".to_string(),
            identifier: "MT-650".to_string(),
            workspace_key: "MT-650".to_string(),
            attempt: 3,
            due_at_ms: ms_from_now(30_000),
            error: Some("no available orchestrator slots".to_string()),
        },
    );
    state.ended_runtime_seconds = 1834.2;
    state
}

async fn start_server(
    path: &Path,
    state: OrchestratorState,
) -> (
    String,
    symphony::observability::http::HttpServerHandle,
    mpsc::UnboundedReceiver<()>,
) {
    let config = EffectiveConfig::load(Some(path.to_path_buf())).unwrap();
    let shared_status = SharedStatus::new(std::slice::from_ref(&config));
    shared_status.publish(&state, &[config]).await;
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

async fn get_json(base: &str, path: &str) -> Value {
    reqwest::get(format!("{base}{path}"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn state_api_exposes_complete_baseline_schema_and_omits_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path, populated_state()).await;

    let response = reqwest::get(format!("{base}/api/v1/state")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    assert!(json["generated_at"].is_string());
    assert_eq!(
        json["counts"],
        json!({"running": 1, "retrying": 1, "sources": 1})
    );
    assert_eq!(json["running"].as_array().unwrap().len(), 1);
    assert_eq!(json["running"][0]["issue_id"], "abc123");
    assert_eq!(json["running"][0]["issue_identifier"], "MT-649");
    assert_eq!(json["running"][0]["state"], "In Progress");
    assert_eq!(json["running"][0]["session_id"], "thread-1-turn-1");
    assert_eq!(json["running"][0]["turn_count"], 0);
    assert_eq!(json["running"][0]["last_event"], "notification");
    assert_eq!(json["running"][0]["last_message"], "Working on tests");
    assert!(json["running"][0]["started_at"].is_string());
    assert!(json["running"][0]["last_event_at"].is_string());
    assert_eq!(
        json["running"][0]["tokens"],
        json!({"input_tokens": 1200, "output_tokens": 800, "total_tokens": 2000})
    );
    assert_eq!(json["retrying"][0]["issue_identifier"], "MT-650");
    assert_eq!(json["retrying"][0]["attempt"], 3);
    assert!(json["retrying"][0]["due_at"].is_string());
    assert!(json["retrying"][0]["due_at_ms"].is_u64());
    assert_eq!(
        json["retrying"][0]["error"],
        "no available orchestrator slots"
    );
    assert_eq!(json["codex_totals"]["input_tokens"], 1200,);
    assert_eq!(json["codex_totals"]["output_tokens"], 800,);
    assert_eq!(json["codex_totals"]["total_tokens"], 2000,);
    assert!(
        json["codex_totals"]["seconds_running"].as_f64().unwrap() >= 1834.2,
        "live runtime must be added to completed runtime"
    );
    assert_eq!(json["rate_limits"], json!({"remaining": 12}));
    assert!(!text.contains("super-secret"));
    let source_text = reqwest::get(format!("{base}/api/v1/sources"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!source_text.contains("super-secret"));
    server.task.abort();
}

#[tokio::test]
async fn issue_detail_exposes_complete_baseline_schema() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path, populated_state()).await;

    let detail = get_json(&base, "/api/v1/MT-649").await;
    assert_eq!(detail["issue_identifier"], "MT-649");
    assert_eq!(detail["issue_id"], "abc123");
    assert_eq!(detail["status"], "running");
    assert_eq!(detail["source_id"], "default");
    assert!(
        detail["workspace"]["path"]
            .as_str()
            .unwrap()
            .ends_with("MT-649")
    );
    assert_eq!(detail["workspace"]["key"], "MT-649");
    assert_eq!(
        detail["attempts"],
        json!({"restart_count": 2, "current_retry_attempt": 2})
    );
    assert_eq!(detail["running"]["session_id"], "thread-1-turn-1");
    assert_eq!(detail["running"]["state"], "In Progress");
    assert_eq!(detail["running"]["last_event"], "notification");
    assert_eq!(detail["running"]["last_message"], "Working on tests");
    assert!(detail["running"]["last_event_at"].is_string());
    assert_eq!(
        detail["running"]["tokens"],
        json!({"input_tokens": 1200, "output_tokens": 800, "total_tokens": 2000})
    );
    assert!(detail["retry"].is_null());
    assert_eq!(detail["logs"]["codex_session_logs"], json!([]));
    assert_eq!(
        detail["recent_events"],
        json!([{
            "at": "2026-02-24T20:14:59Z",
            "event": "notification",
            "message": "Working on tests"
        }])
    );
    assert!(detail["last_error"].is_null());
    assert_eq!(detail["tracked"], json!({}));

    let retry = get_json(&base, "/api/v1/MT-650").await;
    assert_eq!(retry["status"], "retrying");
    assert!(retry["running"].is_null());
    assert_eq!(retry["attempts"]["restart_count"], 3);
    assert_eq!(retry["retry"]["issue_identifier"], "MT-650");
    assert!(retry["retry"]["due_at"].is_string());
    assert_eq!(retry["retry"]["error"], "no available orchestrator slots");
    server.task.abort();
}

#[tokio::test]
async fn known_routes_reject_unsupported_methods_with_json_envelopes() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path, populated_state()).await;
    let client = reqwest::Client::new();

    for (method, path) in [
        (Method::POST, "/api/v1/state"),
        (Method::DELETE, "/api/v1/sources"),
        (Method::POST, "/api/v1/repositories?source_id=default"),
        (Method::GET, "/api/v1/refresh"),
        (Method::PATCH, "/api/v1/MT-649"),
    ] {
        let response = client
            .request(method, format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "method_not_allowed");
        assert!(body["error"]["message"].is_string());
    }

    let unknown = reqwest::get(format!("{base}/api/v1/unknown/path"))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        unknown.json::<Value>().await.unwrap()["error"]["code"],
        "route_not_found"
    );
    server.task.abort();
}

#[tokio::test]
async fn repository_endpoint_is_read_only_and_preserves_workflow() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let before = fs::read(&path).unwrap();
    let (base, server, _refresh_rx) = start_server(&path, OrchestratorState::default()).await;
    let client = reqwest::Client::new();

    let listed = get_json(&base, "/api/v1/repositories?source_id=default").await;
    assert_eq!(
        listed["repositories"],
        json!([{"owner": "octo", "name": "repo"}])
    );
    for request in [
        client
            .post(format!("{base}/api/v1/repositories"))
            .json(&json!({"source_id":"default","owner":"octo","name":"worker"})),
        client.delete(format!(
            "{base}/api/v1/repositories?source_id=default&owner=octo&name=repo"
        )),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "method_not_allowed"
        );
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    server.task.abort();
}

#[tokio::test]
async fn dashboard_consumes_read_only_api_shapes() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path, OrchestratorState::default()).await;

    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Symphony"));
    assert!(html.contains("Repositories"));
    assert!(html.contains("state.codex_totals.seconds_running"));
    assert!(html.contains("api('/api/v1/state')"));
    assert!(html.contains("api('/api/v1/sources')"));
    assert!(html.contains("api('/api/v1/refresh'"));
    assert!(!html.contains("repo-form"));
    assert!(!html.contains("api('/api/v1/repositories"));
    assert!(!html.contains("workflow_store"));
    server.task.abort();
}

#[tokio::test]
async fn refresh_route_returns_documented_payload_and_coalesces() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, mut refresh_rx) = start_server(&path, OrchestratorState::default()).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base}/api/v1/refresh"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first: Value = first.json().await.unwrap();
    let requested_at = first["requested_at"].clone();
    assert_eq!(
        first,
        json!({
            "queued": true,
            "coalesced": false,
            "requested_at": requested_at,
            "operations": ["poll", "reconcile"]
        })
    );
    assert!(first["requested_at"].is_string());
    refresh_rx.recv().await.unwrap();

    let second = client
        .post(format!("{base}/api/v1/refresh"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second: Value = second.json().await.unwrap();
    assert_eq!(second["queued"], true);
    assert_eq!(second["coalesced"], true);
    assert!(second["requested_at"].is_string());
    assert_eq!(second["operations"], json!(["poll", "reconcile"]));
    assert!(refresh_rx.try_recv().is_err());
    server.task.abort();
}

#[tokio::test]
async fn unknown_issue_identifier_returns_json_not_found_envelope() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let (base, server, _refresh_rx) = start_server(&path, OrchestratorState::default()).await;

    let response = reqwest::get(format!("{base}/api/v1/MT-999")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "issue_not_found"
    );
    server.task.abort();
}
