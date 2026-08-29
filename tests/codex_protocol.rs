use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use symphony::agent::codex::{CodexAppServerClient, CodexClient};
use symphony::config::CodexConfig;
use symphony::domain::CodexEvent;
use tempfile::TempDir;
use tokio::time::{Duration, Instant, sleep, timeout};

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

log({"cwd": os.getcwd(), "pid": os.getpid()})
init = recv()
if scenario == "startup_timeout":
    log({"startup_request_received": True})
    time.sleep(10)
    raise SystemExit(0)
if scenario == "startup_unrelated_messages":
    log({"startup_request_received": True})
    request_id = 1000
    while True:
        send({"id": request_id, "method": "item/tool/call", "params": {"tool": "unknown"}})
        response = recv()
        if response is None:
            break
        log({"unrelated_response": response})
        request_id += 1
    raise SystemExit(0)
send({"id": init["id"], "result": {"codexHome": os.getcwd(), "authMode": "none"}})
initialized = recv()
thread = recv()
log({"thread_params": thread.get("params")})
send({"id": thread["id"], "result": {"thread": {"id": "thread-1"}, "approvalPolicy": "never", "approvalsReviewer": "auto", "cwd": os.getcwd(), "model": "fake", "modelProvider": "fake", "sandbox": {"type": "dangerFullAccess"}}})

turn_number = 0
while True:
    turn = recv()
    if turn is None:
        break
    turn_number += 1
    turn_id = f"turn-{turn_number}"
    log({"turn_params": turn.get("params"), "turn_started": turn_id})
    send({"id": turn["id"], "result": {"turn": {"id": turn_id, "status": "inProgress", "items": []}}})

    if scenario == "complete":
        send({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-1","turnId":turn_id,"tokenUsage":{"last":{"inputTokens":1,"outputTokens":2,"totalTokens":3,"cachedInputTokens":0,"reasoningOutputTokens":0},"total":{"inputTokens":10,"outputTokens":20,"totalTokens":30,"cachedInputTokens":0,"reasoningOutputTokens":0}}}})
        send({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":42}}}})
        send({"method":"notice","params":{"message":"hello"}})
        send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":turn_id,"status":"completed","items":[]}}})
    elif scenario == "timeout":
        time.sleep(2)
    elif scenario == "approval":
        send({"id":"cmd-1","method":"item/commandExecution/requestApproval","params":{"itemId":"item-1","threadId":"thread-1","turnId":turn_id}})
        log({"approval_response": recv()})
        send({"id":"file-1","method":"item/fileChange/requestApproval","params":{"itemId":"item-2","threadId":"thread-1","turnId":turn_id}})
        log({"file_response": recv()})
        send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":turn_id,"status":"completed","items":[]}}})
    elif scenario == "dynamic":
        send({"id":"tool-1","method":"item/tool/call","params":{"threadId":"thread-1","turnId":turn_id,"tool":"unknown","callId":"call-1","arguments":{}}})
        log({"tool_response": recv()})
        send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":turn_id,"status":"completed","items":[]}}})
    elif scenario == "user_input":
        send({"id":"input-1","method":"item/tool/requestUserInput","params":{"itemId":"item-3","threadId":"thread-1","turnId":turn_id,"questions":[]}})
        log({"input_response": recv()})
        break
    elif scenario == "failed":
        send({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":turn_id,"status":"failed","items":[],"error":{"message":"boom"}}}})
    elif scenario == "malformed":
        sys.stdout.write("not-json\n")
        sys.stdout.flush()
        break
    elif scenario == "oversized":
        sys.stdout.write('{"method":"notice","payload":"' + ("x" * (10 * 1024 * 1024 + 1)) + '"}\n')
        sys.stdout.flush()
        break
    elif scenario == "hang":
        time.sleep(30)
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
        "export FAKE_CODEX_LOG={}; exec python3 {} {}",
        shell_quote_path(&log_path),
        shell_quote_path(&script_path),
        shell_quote(scenario)
    );
    Harness {
        _temp: temp,
        workspace,
        log_path,
        command,
    }
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&bash_path(path))
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn bash_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let bytes = path.as_bytes();
    assert!(
        bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/',
        "temporary path must be a local Windows drive: {path}"
    );
    format!(
        "/mnt/{}/{}",
        char::from(bytes[0]).to_ascii_lowercase(),
        &path[3..]
    )
}

#[cfg(not(windows))]
fn bash_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn config(command: String) -> CodexConfig {
    CodexConfig {
        command,
        approval_policy: Some(json!("never")),
        thread_sandbox: Some(json!("danger-full-access")),
        turn_sandbox_policy: Some(json!({ "type": "dangerFullAccess" })),
        turn_timeout_ms: 5_000,
        read_timeout_ms: 5_000,
        stall_timeout_ms: 0,
    }
}

async fn run_scenario(scenario: &str) -> (Harness, symphony::Result<Vec<CodexEvent>>) {
    let harness = harness(scenario);
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };
    let result = async {
        let mut session = client.start_session(&workspace_path, &mut on_event).await?;
        let outcome = session.run_turn("do the work").await;
        session.shutdown().await;
        drop(session);
        outcome?;
        Ok(events.lock().expect("events mutex").clone())
    }
    .await;
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
    let bash_workspace_path = bash_path(&workspace_path);
    assert_eq!(log[0]["cwd"], bash_workspace_path.as_str());
    let received: Vec<&Value> = log
        .iter()
        .filter_map(|entry| entry.get("received"))
        .collect();
    assert_eq!(received[0]["method"], "initialize");
    assert_eq!(received[1]["method"], "initialized");
    assert_eq!(received[2]["method"], "thread/start");
    assert_eq!(received[3]["method"], "turn/start");
    assert_eq!(received[2]["params"]["cwd"], bash_workspace_path.as_str());
    assert_eq!(received[2]["params"]["approvalPolicy"], "never");
    assert_eq!(received[2]["params"]["sandbox"], "danger-full-access");
    assert_eq!(received[3]["params"]["cwd"], bash_workspace_path.as_str());
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
async fn continuations_reuse_one_process_and_thread_with_per_turn_sessions() {
    let harness = harness("complete");
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };
    let mut session = client
        .start_session(&workspace_path, &mut on_event)
        .await
        .expect("session starts");
    let first = session
        .run_turn("first")
        .await
        .expect("first turn completes");
    let second = session
        .run_turn("second")
        .await
        .expect("second turn completes");
    session.shutdown().await;
    drop(session);

    assert_eq!(first.thread_id, "thread-1");
    assert_eq!(second.thread_id, first.thread_id);
    assert_ne!(first.turn_id, second.turn_id);
    assert_ne!(first.session_id, second.session_id);
    let log = log_entries(&harness.log_path);
    let received: Vec<&Value> = log
        .iter()
        .filter_map(|entry| entry.get("received"))
        .collect();
    assert_eq!(
        received
            .iter()
            .filter(|message| message["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        received
            .iter()
            .filter(|message| message["method"] == "thread/start")
            .count(),
        1
    );
    let turns: Vec<&Value> = received
        .iter()
        .filter(|message| message["method"] == "turn/start")
        .copied()
        .collect();
    assert_eq!(turns.len(), 2);
    assert!(
        turns
            .iter()
            .all(|turn| turn["params"]["threadId"] == "thread-1")
    );
    let session_ids: Vec<String> = events
        .lock()
        .expect("events mutex")
        .iter()
        .filter(|event| event.event == "session_started")
        .filter_map(|event| event.session_id.clone())
        .collect();
    assert_eq!(session_ids, vec![first.session_id, second.session_id]);
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
    let (harness, result) = run_scenario("timeout").await;
    let error = result.expect_err("timeout should fail").to_string();
    assert!(error.contains("timeout"), "{error}");
    assert!(
        log_entries(&harness.log_path)
            .iter()
            .any(|entry| entry.get("turn_started").is_some()),
        "the fake app-server must receive turn/start before the timeout"
    );
}

#[tokio::test]
async fn auto_approves_command_and_file_requests_for_session() {
    let (harness, result) = run_scenario("approval").await;
    let events = result.expect("run completes");
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
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == "approval_auto_approved")
            .count(),
        2
    );
}

#[tokio::test]
async fn unsupported_dynamic_tool_calls_return_unsuccessful_response() {
    let (harness, result) = run_scenario("dynamic").await;
    let events = result.expect("run completes");
    let log = log_entries(&harness.log_path);
    let response = log
        .iter()
        .find_map(|entry| entry.get("tool_response"))
        .expect("tool response");
    assert_eq!(
        response["result"],
        json!({ "success": false, "contentItems": [] })
    );
    assert!(
        events
            .iter()
            .any(|event| event.event == "unsupported_tool_call")
    );
}

#[tokio::test]
async fn user_input_required_fails_without_stalling() {
    let harness = harness("user_input");
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };
    let mut session = client
        .start_session(&workspace_path, &mut on_event)
        .await
        .expect("session starts");
    let error = session
        .run_turn("input")
        .await
        .expect_err("user input fails")
        .to_string();
    session.shutdown().await;
    drop(session);

    assert!(error.contains("user_input_required"), "{error}");
    assert!(
        log_entries(&harness.log_path)
            .iter()
            .any(|entry| entry.get("input_response").is_some())
    );
    assert!(
        events
            .lock()
            .expect("events mutex")
            .iter()
            .any(|event| event.event == "turn_input_required")
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

#[tokio::test]
async fn startup_failures_emit_normalized_events() {
    let workspace = tempfile::tempdir().expect("workspace");
    let client = CodexAppServerClient::new(config("exit 1".to_string()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };

    let error = match client.start_session(workspace.path(), &mut on_event).await {
        Ok(_) => panic!("startup must fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("process_exit"), "{error}");
    assert!(
        events
            .lock()
            .expect("events mutex")
            .iter()
            .any(|event| event.event == "startup_failed")
    );
}

#[tokio::test]
async fn startup_response_wait_uses_the_absolute_turn_deadline() {
    let harness = harness("startup_timeout");
    let mut codex_config = config(harness.command.clone());
    codex_config.read_timeout_ms = 10_000;
    codex_config.turn_timeout_ms = 5_000;
    let client = CodexAppServerClient::new(codex_config);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };

    let error = match client
        .start_session(harness.workspace.path(), &mut on_event)
        .await
    {
        Ok(_) => panic!("startup response must time out"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("initialize response"), "{error}");
    assert!(
        log_entries(&harness.log_path)
            .iter()
            .any(|entry| entry.get("startup_request_received") == Some(&Value::Bool(true))),
        "the fake app-server must receive initialize before the response deadline"
    );
    assert!(events.lock().expect("events mutex").iter().any(|event| {
        event.event == "startup_failed"
            && event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("initialize response"))
    }));
}

#[tokio::test]
async fn unrelated_startup_messages_do_not_extend_the_response_deadline() {
    let harness = harness("startup_unrelated_messages");
    let mut codex_config = config(harness.command.clone());
    codex_config.turn_timeout_ms = 10_000;
    codex_config.stall_timeout_ms = 5_000;
    let deadline = Duration::from_millis(codex_config.stall_timeout_ms as u64);
    let client = CodexAppServerClient::new(codex_config);
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let log_path = harness.log_path.clone();
    let task = tokio::spawn(async move {
        let mut on_event = |_| {};
        match client.start_session(&workspace_path, &mut on_event).await {
            Ok(mut session) => {
                session.shutdown().await;
                panic!("startup response must time out");
            }
            Err(error) => error.to_string(),
        }
    });
    timeout(Duration::from_secs(10), async {
        loop {
            if fs::read_to_string(&log_path)
                .is_ok_and(|content| content.contains(r#""startup_request_received":true"#))
            {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake app-server receives initialize");

    let started = Instant::now();
    let error = task.await.expect("startup task completes");
    let elapsed = started.elapsed();

    assert!(error.contains("initialize response"), "{error}");
    assert!(
        elapsed <= deadline + Duration::from_millis(800),
        "startup took {elapsed:?}, exceeding the {deadline:?} response deadline"
    );
    let log = log_entries(&harness.log_path);
    assert!(
        log.iter()
            .any(|entry| entry.get("unrelated_response").is_some()),
        "the client must keep handling server requests while awaiting initialize"
    );
    let pid = log
        .iter()
        .find_map(|entry| entry.get("pid").and_then(Value::as_u64))
        .expect("fake app-server pid");
    for _ in 0..100 {
        if !process_is_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("app-server process {pid} remained alive after startup timeout");
}

#[tokio::test]
async fn oversized_jsonl_messages_fail_with_a_malformed_event() {
    let harness = harness("oversized");
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let mut on_event = move |event| {
        captured.lock().expect("events mutex").push(event);
    };
    let mut session = client
        .start_session(&workspace_path, &mut on_event)
        .await
        .expect("session starts");
    let error = session
        .run_turn("oversized")
        .await
        .expect_err("oversized JSONL line fails")
        .to_string();
    session.shutdown().await;
    drop(session);

    assert!(error.contains("protocol_error"), "{error}");
    assert!(
        events
            .lock()
            .expect("events mutex")
            .iter()
            .any(|event| event.event == "malformed")
    );
}

#[tokio::test]
async fn aborting_worker_task_terminates_the_app_server() {
    let harness = harness("hang");
    let log_path = harness.log_path.clone();
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let client = CodexAppServerClient::new(config(harness.command.clone()));
    let task = tokio::spawn(async move {
        let mut on_event = |_| {};
        let mut session = client.start_session(&workspace_path, &mut on_event).await?;
        session.run_turn("hang").await
    });

    let pid = timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(content) = fs::read_to_string(&log_path)
                && let Some(pid) = content.lines().find_map(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|entry| entry.get("pid").and_then(Value::as_u64))
                })
            {
                return pid;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake app-server starts");
    task.abort();
    let _ = task.await;
    for _ in 0..100 {
        if !process_is_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("app-server process {pid} remained alive after worker cancellation");
}

fn process_is_alive(pid: u64) -> bool {
    Command::new("bash")
        .arg("-lc")
        .arg(format!("kill -0 {pid}"))
        .status()
        .expect("run bash")
        .success()
}
