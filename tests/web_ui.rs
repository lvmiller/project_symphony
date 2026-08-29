use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::{TimeZone, Utc};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use symphony::config::{EffectiveConfig, ServerConfig};
use symphony::domain::{CodexEvent, ExecutionTarget, Issue, RetryEntry, TokenTotals};
use symphony::observability::http::{SharedStatus, spawn_http_server};
use symphony::orchestrator::state::{OrchestratorState, RECENT_EVENT_MESSAGE_LIMIT_BYTES};
use symphony::time::ms_from_now;
use tokio::sync::mpsc;

fn workflow() -> &'static str {
    "---\ntracker:\n  kind: github\n  api_key: super-secret\n  repository:\n    owner: octo\n    name: repo\n  project:\n    owner_login: octo\n    number: 7\nhooks:\n  after_create: echo hook-secret\n---\nprompt-secret\n"
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
            execution_target: ExecutionTarget::Local,
            workspace_path: PathBuf::new(),
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
    let server_config = EffectiveConfig::load(Some(path.to_path_buf()))
        .unwrap()
        .server;
    start_server_with_config(path, state, server_config).await
}

async fn start_server_with_config(
    path: &Path,
    state: OrchestratorState,
    server_config: symphony::config::ServerConfig,
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
        &server_config,
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
    assert_eq!(json["running"][0]["thread_id"], "thread-1");
    assert_eq!(json["running"][0]["turn_id"], "turn-1");
    assert_eq!(json["running"][0]["codex_app_server_pid"], 42);
    assert_eq!(json["running"][0]["workspace_key"], "MT-649");
    assert_eq!(json["running"][0]["retry_attempt"], 2);
    assert_eq!(json["running"][0]["cancel_requested"], false);
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
    assert!(json["retrying"][0]["remaining_delay_ms"].is_u64());
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
    for excluded in ["super-secret", "prompt-secret", "hook-secret"] {
        assert!(!text.contains(excluded));
    }
    let source_text = reqwest::get(format!("{base}/api/v1/sources"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for excluded in ["super-secret", "prompt-secret", "hook-secret"] {
        assert!(!source_text.contains(excluded));
    }
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
    assert_eq!(detail["running"]["thread_id"], "thread-1");
    assert_eq!(detail["running"]["turn_id"], "turn-1");
    assert_eq!(detail["running"]["codex_app_server_pid"], 42);
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
    assert!(retry["retry"]["remaining_delay_ms"].is_u64());
    assert_eq!(retry["retry"]["error"], "no available orchestrator slots");
    server.task.abort();
}

#[tokio::test]
async fn issue_detail_exposes_chronological_bounded_recent_event_summaries() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let mut state = populated_state();
    state.apply_codex_event(CodexEvent {
        issue_id: "abc123".to_string(),
        event: "progress".to_string(),
        timestamp: Utc
            .with_ymd_and_hms(2026, 2, 24, 20, 15, 0)
            .single()
            .unwrap(),
        session_id: Some("thread-1-turn-1".to_string()),
        thread_id: Some("thread-1".to_string()),
        turn_id: Some("turn-1".to_string()),
        codex_app_server_pid: Some(42),
        message: Some("Inspecting workspace".to_string()),
        absolute_token_totals: None,
        rate_limits: None,
    });
    state.apply_codex_event(CodexEvent {
        issue_id: "abc123".to_string(),
        event: "progress".to_string(),
        timestamp: Utc
            .with_ymd_and_hms(2026, 2, 24, 20, 15, 1)
            .single()
            .unwrap(),
        session_id: Some("thread-1-turn-1".to_string()),
        thread_id: Some("thread-1".to_string()),
        turn_id: Some("turn-1".to_string()),
        codex_app_server_pid: Some(42),
        message: Some("x".repeat(RECENT_EVENT_MESSAGE_LIMIT_BYTES + 1)),
        absolute_token_totals: None,
        rate_limits: None,
    });
    let (base, server, _refresh_rx) = start_server(&path, state).await;

    let detail = get_json(&base, "/api/v1/MT-649").await;
    let recent_events = detail["recent_events"].as_array().unwrap();
    assert_eq!(recent_events.len(), 3);
    assert_eq!(recent_events[0]["event"], "notification");
    assert_eq!(recent_events[1]["message"], "Inspecting workspace");
    assert_eq!(
        recent_events[2]["message"].as_str().unwrap().len(),
        RECENT_EVENT_MESSAGE_LIMIT_BYTES
    );
    assert!(
        recent_events[2]["message"]
            .as_str()
            .unwrap()
            .ends_with("...")
    );
    for event in recent_events {
        assert_eq!(event.as_object().unwrap().len(), 3);
        assert!(event.get("rate_limits").is_none());
        assert!(event.get("absolute_token_totals").is_none());
    }
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
        .header("content-type", "application/json")
        .header("origin", &base)
        .body("{}")
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
        .header("content-type", "application/json")
        .header("origin", &base)
        .body("{}")
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
async fn authenticated_refresh_rejects_csrf_and_enforces_cooldown_without_leaking_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_workflow(temp.path());
    let config = EffectiveConfig::load(Some(path)).unwrap();
    let shared_status = SharedStatus::new(std::slice::from_ref(&config));
    shared_status.publish(&populated_state(), &[config]).await;
    let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel();
    let refresh_pending = Arc::new(AtomicBool::new(false));
    let server_config = ServerConfig {
        auth_token: Some("operator-secret".to_string()),
        refresh_cooldown_ms: 60_000,
        ..ServerConfig::default()
    };
    let server = spawn_http_server(
        "127.0.0.1:0".parse().unwrap(),
        shared_status,
        refresh_tx,
        refresh_pending.clone(),
        &server_config,
    )
    .await
    .unwrap();
    let base = format!("http://{}", server.local_addr);
    let client = reqwest::Client::new();

    for request in [
        client.get(format!("{base}/api/v1/state")),
        client
            .get(format!("{base}/api/v1/state"))
            .header("authorization", "Basic malformed"),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "authentication_required");
        assert!(body.get("counts").is_none());
        assert!(!body.to_string().contains("operator-secret"));
    }

    let simple_form = client
        .post(format!("{base}/api/v1/refresh"))
        .header("authorization", "Bearer operator-secret")
        .header("origin", &base)
        .form(&[("refresh", "now")])
        .send()
        .await
        .unwrap();
    assert_eq!(simple_form.status(), StatusCode::FORBIDDEN);
    let simple_body = simple_form.json::<Value>().await.unwrap();
    assert_eq!(simple_body["error"]["code"], "invalid_refresh_request");
    assert!(simple_body.get("counts").is_none());
    assert!(!simple_body.to_string().contains("operator-secret"));
    assert!(refresh_rx.try_recv().is_err());
    let missing_origin = client
        .post(format!("{base}/api/v1/refresh"))
        .header("authorization", "Bearer operator-secret")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        missing_origin.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_refresh_request"
    );
    assert!(refresh_rx.try_recv().is_err());

    let cross_origin = client
        .post(format!("{base}/api/v1/refresh"))
        .header("authorization", "Bearer operator-secret")
        .header("content-type", "application/json")
        .header("origin", "http://attacker.invalid")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        cross_origin.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_refresh_request"
    );
    assert!(refresh_rx.try_recv().is_err());

    let accepted = client
        .post(format!("{base}/api/v1/refresh"))
        .header("authorization", "Bearer operator-secret")
        .header("content-type", "application/json")
        .header("origin", &base)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    assert_eq!(refresh_rx.recv().await, Some(()));
    assert!(refresh_rx.try_recv().is_err());

    refresh_pending.store(false, std::sync::atomic::Ordering::Release);
    let rate_limited = client
        .post(format!("{base}/api/v1/refresh"))
        .header("authorization", "Bearer operator-secret")
        .header("content-type", "application/json")
        .header("origin", &base)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let rate_limited: Value = rate_limited.json().await.unwrap();
    assert_eq!(rate_limited["error"]["code"], "refresh_rate_limited");
    assert!(
        rate_limited["retry_after_ms"]
            .as_u64()
            .is_some_and(|ms| ms > 0)
    );
    assert!(refresh_rx.try_recv().is_err());

    server.task.abort();
}

#[tokio::test]
async fn non_loopback_server_startup_requires_authentication() {
    let (refresh_tx, _refresh_rx) = mpsc::unbounded_channel();
    let result = spawn_http_server(
        "0.0.0.0:0".parse().unwrap(),
        SharedStatus::new(&[]),
        refresh_tx,
        Arc::new(AtomicBool::new(false)),
        &ServerConfig::default(),
    )
    .await;
    let error = match result {
        Ok(server) => {
            server.task.abort();
            panic!("non-loopback server started without authentication");
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("http_auth_required"));
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
