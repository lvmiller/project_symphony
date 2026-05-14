use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use symphony::agent::codex::{CodexAppServerClient, CodexClient};
use symphony::config::CodexConfig;
use symphony::domain::CodexEvent;
use tempfile::TempDir;

const FAKE_CODEX: &str = r#"#!/usr/bin/env python3
import json
import os
import sys
import time

scenario = sys.argv[1]
log_path = os.environ["FAKE_CODEX_LOG"]

def log(value):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, separators=(",", ":")) + "\n")

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def recv():
    line = sys.stdin.readline()
    if not line:
        return None
    value = json.loads(line)
    log({"received": value})
    return value

log({"cwd": os.getcwd()})
init = recv()
send({"id": init["id"], "result": {"codexHome": os.getcwd(), "authMode": "none"}})
initialized = recv()
thread = recv()
log({"thread_params": thread.get("params")})
send({"id": thread["id"], "result": {"thread": {"id": "thread-1"}, "approvalPolicy": "never", "approvalsReviewer": "auto", "cwd": os.getcwd(), "model": "fake", "modelProvider": "fake", "sandbox": {"type": "dangerFullAccess"}}})
turn = recv()
log({"turn_params": turn.get("params")})
send({"id": turn["id"], "result": {"turn": {"id": "turn-1", "status": "inProgress", "items": []}}})

if scenario == "complete":
    send({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":"turn-1","tokenUsage":{"last":{"inputTokens":1,"outputTokens":2,"totalTokens":3,"cachedInputTokens":0,"reasoningOutputTokens":0},"total":{"inputTokens":10,"outputTokens":20,"totalTokens":30,"cachedInputTokens":0,"reasoningOutputTokens":0}}}})
    send({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":42}}}})
    send({"method":"notice","params":{"message":"hello"}})
    send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}})
elif scenario == "timeout":
    time.sleep(2)
elif scenario == "approval":
    send({"id":"cmd-1","method":"item/commandExecution/requestApproval","params":{"itemId":"item-1","threadId":"thread-1","turnId":"turn-1"}})
    log({"approval_response": recv()})
    send({"id":"file-1","method":"item/fileChange/requestApproval","params":{"itemId":"item-2","threadId":"thread-1","turnId":"turn-1"}})
    log({"file_response": recv()})
    send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}})
elif scenario == "dynamic":
    send({"id":"tool-1","method":"item/tool/call","params":{"threadId":"thread-1","turnId":"turn-1","tool":"unknown","callId":"call-1","arguments":{}}})
    log({"tool_response": recv()})
    send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[]}}})
elif scenario == "user_input":
    send({"id":"input-1","method":"item/tool/requestUserInput","params":{"itemId":"item-3","threadId":"thread-1","turnId":"turn-1","questions":[]}})
    log({"input_response": recv()})
elif scenario == "failed":
    send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"failed","items":[],"error":{"message":"boom"}}}})
elif scenario == "malformed":
    sys.stdout.write("not-json\n")
    sys.stdout.flush()
else:
    raise SystemExit(f"unknown scenario: {scenario}")
"#;

struct Harness {
    _temp: TempDir,
    workspace: tempfile::TempDir,
    log_path: std::path::PathBuf,
    command: String,
}

fn harness(scenario: &str) -> Harness {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("workspace");
    let script_path = temp.path().join("fake_codex.py");
    fs::write(&script_path, FAKE_CODEX).expect("write fake script");
    let log_path = temp.path().join("fake.log");
    let command = format!(
        "FAKE_CODEX_LOG={} python3 {} {}",
        shell_quote(&log_path),
        shell_quote(&script_path),
        shell_quote(Path::new(scenario))
    );
    Harness {
        _temp: temp,
        workspace,
        log_path,
        command,
    }
}

fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn config(command: String) -> CodexConfig {
    CodexConfig {
        command,
        approval_policy: Some(json!("never")),
        thread_sandbox: Some(json!("danger-full-access")),
        turn_sandbox_policy: Some(json!({ "type": "dangerFullAccess" })),
        turn_timeout_ms: 300,
        read_timeout_ms: 100,
        stall_timeout_ms: 0,
    }
}

async fn run_scenario(scenario: &str) -> (Harness, symphony::Result<Vec<CodexEvent>>) {
    let harness = harness(scenario);
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let result = client
        .run_turn(&workspace_path, "do the work", &mut move |event| {
            captured.lock().expect("events mutex").push(event);
        })
        .await
        .map(|_| events.lock().expect("events mutex").clone());
    (harness, result)
}

fn log_entries(path: &Path) -> Vec<Value> {
    let content = fs::read_to_string(path).expect("read log");
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("json log line"))
        .collect()
}

#[tokio::test]
async fn sends_schema_shaped_startup_messages_and_streams_completion() {
    let (harness, result) = run_scenario("complete").await;
    let events = result.expect("run completes");
    let log = log_entries(&harness.log_path);

    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    assert_eq!(log[0]["cwd"], workspace_path.to_string_lossy().as_ref());
    let received: Vec<&Value> = log
        .iter()
        .filter_map(|entry| entry.get("received"))
        .collect();
    assert_eq!(received[0]["method"], "initialize");
    assert_eq!(received[1]["method"], "initialized");
    assert_eq!(received[2]["method"], "thread/start");
    assert_eq!(received[3]["method"], "turn/start");
    assert_eq!(
        received[2]["params"]["cwd"],
        workspace_path.to_string_lossy().as_ref()
    );
    assert_eq!(received[2]["params"]["approvalPolicy"], "never");
    assert_eq!(received[2]["params"]["sandbox"], "danger-full-access");
    assert_eq!(
        received[3]["params"]["cwd"],
        workspace_path.to_string_lossy().as_ref()
    );
    assert_eq!(
        received[3]["params"]["sandboxPolicy"],
        json!({"type":"dangerFullAccess"})
    );
    assert_eq!(
        received[3]["params"]["input"],
        json!([{ "type": "text", "text": "do the work" }])
    );

    assert!(events.iter().any(|event| event.event == "session_started"
        && event.session_id.as_deref() == Some("thread-1-turn-1")));
    assert!(events.iter().any(|event| event.event == "turn_started"));
    assert!(events.iter().any(|event| event.event == "turn_completed"));
    assert!(
        events.iter().any(
            |event| event.event == "notification" && event.message.as_deref() == Some("notice")
        )
    );
}

#[tokio::test]
async fn extracts_token_totals_and_rate_limits() {
    let (_harness, result) = run_scenario("complete").await;
    let events = result.expect("run completes");
    let totals = events
        .iter()
        .find_map(|event| event.absolute_token_totals.as_ref())
        .expect("token totals event");
    assert_eq!(totals.input_tokens, 10);
    assert_eq!(totals.output_tokens, 20);
    assert_eq!(totals.total_tokens, 30);
    let rate_limits = events
        .iter()
        .find_map(|event| event.rate_limits.as_ref())
        .expect("rate limit event");
    assert_eq!(rate_limits["limitId"], "codex");
    assert_eq!(rate_limits["primary"]["usedPercent"], 42);
}

#[tokio::test]
async fn times_out_when_turn_does_not_complete() {
    let (_harness, result) = run_scenario("timeout").await;
    let error = result.expect_err("timeout should fail").to_string();
    assert!(error.contains("timeout"), "{error}");
}

#[tokio::test]
async fn auto_approves_command_and_file_requests_for_session() {
    let (harness, result) = run_scenario("approval").await;
    result.expect("run completes");
    let log = log_entries(&harness.log_path);
    let command = log
        .iter()
        .find_map(|entry| entry.get("approval_response"))
        .expect("command response");
    let file = log
        .iter()
        .find_map(|entry| entry.get("file_response"))
        .expect("file response");
    assert_eq!(command["result"]["decision"], "acceptForSession");
    assert_eq!(file["result"]["decision"], "acceptForSession");
}

#[tokio::test]
async fn unsupported_dynamic_tool_calls_return_unsuccessful_response() {
    let (harness, result) = run_scenario("dynamic").await;
    result.expect("run completes");
    let log = log_entries(&harness.log_path);
    let response = log
        .iter()
        .find_map(|entry| entry.get("tool_response"))
        .expect("tool response");
    assert_eq!(
        response["result"],
        json!({ "success": false, "contentItems": [] })
    );
}

#[tokio::test]
async fn user_input_required_fails_without_stalling() {
    let (harness, result) = run_scenario("user_input").await;
    let error = result.expect_err("user input should fail").to_string();
    assert!(error.contains("user_input_required"), "{error}");
    let log = log_entries(&harness.log_path);
    assert!(
        log.iter()
            .any(|entry| entry.get("input_response").is_some())
    );
}

#[tokio::test]
async fn malformed_messages_are_reported_and_fail_the_run() {
    let (_harness, result) = run_scenario("malformed").await;
    let error = result
        .expect_err("malformed message should fail")
        .to_string();
    assert!(error.contains("protocol_error"), "{error}");
}
