use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use symphony::agent::codex::{CodexAppServerClient, CodexClient};
use symphony::config::{
    CodexConfig, GithubConfig, GithubProjectOwnerType, GithubRepositoryConfig, TrackerConfig,
};
use symphony::domain::CodexEvent;
use symphony::tracker::github::GitHubGraphqlExecutor;
use tempfile::TempDir;
use tokio::time::{Duration, Instant, sleep, timeout};

const COMPATIBILITY_FIXTURE: &str = include_str!("fixtures/codex-app-server-v2/compatibility.json");

const FAKE_CODEX: &str = r#"#!/usr/bin/env python3
import json
import os
import sys
import time

scenario = sys.argv[1]
log_path = os.environ["FAKE_CODEX_LOG"]
with open(os.environ["FAKE_CODEX_FIXTURE"], encoding="utf-8") as handle:
    fixture = json.load(handle)

def fresh_fixture(*keys):
    value = fixture
    for key in keys:
        value = value[key]
    return json.loads(json.dumps(value))

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
        send({"id": request_id, "method": "item/tool/call", "params": {"threadId": "thread-1", "turnId": "turn-1", "tool": "unknown", "callId": "call-1", "arguments": {}}})
        response = recv()
        if response is None:
            break
        log({"unrelated_response": response})
        request_id += 1
    raise SystemExit(0)
if scenario == "startup_missing_id":
    send({"result": fresh_fixture("startup", "initialize_result")})
    time.sleep(30)
if scenario == "startup_missing_field":
    result = fresh_fixture("startup", "initialize_result")
    del result["authMode"]
    send({"id": init["id"], "result": result})
    time.sleep(30)
if scenario == "startup_bad_id":
    send({"id": {}, "result": fresh_fixture("startup", "initialize_result")})
    time.sleep(30)
send({"id": str(init["id"]) if scenario == "string_response_ids" else init["id"], "result": fresh_fixture("startup", "initialize_result")})
initialized = recv()
thread = recv()
log({"thread_params": thread.get("params")})
if scenario == "thread_missing_id":
    send({"id": thread["id"], "result": {"thread": {}}})
    time.sleep(30)
send({"id": str(thread["id"]) if scenario == "string_response_ids" else thread["id"], "result": fresh_fixture("startup", "thread_start_result")})

turn_number = 0
while True:
    turn = recv()
    if turn is None:
        break
    turn_number += 1
    turn_id = f"turn-{turn_number}"
    log({"turn_params": turn.get("params"), "turn_started": turn_id})
    start_result = fresh_fixture("turn", "turn_start_result")
    start_result["turn"]["id"] = turn_id
    send({"id": str(turn["id"]) if scenario == "string_response_ids" else turn["id"], "result": start_result})

    if scenario in ("complete", "string_response_ids"):
        token_usage = fresh_fixture("turn", "token_usage_notification")
        token_usage["params"]["turnId"] = turn_id
        send(token_usage)
        send(fresh_fixture("turn", "rate_limits_notification"))
        send({"method":"notice","params":{"message":"hello"}})
        completed = fresh_fixture("turn", "completed_notification")
        completed["params"]["turn"]["id"] = turn_id
        send(completed)
    elif scenario == "timeout":
        time.sleep(2)
    elif scenario == "approval":
        command_approval = fresh_fixture("server_requests", "command_approval")
        command_approval["id"] = 7
        command_approval["params"]["turnId"] = turn_id
        send(command_approval)
        log({"approval_response": recv()})
        file_approval = fresh_fixture("server_requests", "file_approval")
        file_approval["params"]["turnId"] = turn_id
        send(file_approval)
        log({"file_response": recv()})
        completed = fresh_fixture("turn", "completed_notification")
        completed["params"]["turn"]["id"] = turn_id
        send(completed)
    elif scenario.startswith("tool_"):
        if scenario == "tool_unknown":
            tool_call = fresh_fixture("server_requests", "tool_call")
        else:
            arguments = {
                "query": "query($owner: String!) { viewer { login } }",
                "variables": {"owner": "octo-org"},
            }
            if scenario == "tool_invalid":
                arguments = {"query": "query First { viewer { login } } query Second { viewer { login } }"}
            elif scenario == "tool_invalid_variables":
                arguments = {"query": "query { viewer { login } }", "variables": []}
            tool_call = {
                "id": "tool-1",
                "method": "item/tool/call",
                "params": {
                    "threadId": "thread-1",
                    "turnId": turn_id,
                    "namespace": "github_graphql",
                    "tool": "query",
                    "callId": "call-1",
                    "arguments": arguments,
                },
            }
        tool_call["params"]["turnId"] = turn_id
        send(tool_call)
        log({"tool_response": recv()})
        completed = fresh_fixture("turn", "completed_notification")
        completed["params"]["turn"]["id"] = turn_id
        send(completed)
    elif scenario == "user_input":
        user_input = fresh_fixture("server_requests", "user_input")
        user_input["params"]["turnId"] = turn_id
        send(user_input)
        log({"input_response": recv()})
        break
    elif scenario == "unknown_request":
        send({"id": "unknown-1", "method": "item/unknown", "params": {}})
        log({"unknown_response": recv()})
        completed = fresh_fixture("turn", "completed_notification")
        completed["params"]["turn"]["id"] = turn_id
        send(completed)
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
    elif scenario == "bad_status":
        completed = fresh_fixture("turn", "completed_notification")
        completed["params"]["turn"]["id"] = turn_id
        completed["params"]["turn"]["status"] = "queued"
        send(completed)
        time.sleep(30)
    elif scenario == "bad_usage":
        token_usage = fresh_fixture("turn", "token_usage_notification")
        token_usage["params"]["tokenUsage"]["total"]["inputTokens"] = "ten"
        send(token_usage)
        time.sleep(30)
    elif scenario == "bad_rate_limits":
        send({"method": "account/rateLimits/updated", "params": {"rateLimits": []}})
        time.sleep(30)
    elif scenario == "missing_request_id":
        send({"method": "item/tool/call", "params": {"threadId": "thread-1", "turnId": turn_id, "tool": "unknown", "callId": "call-1", "arguments": {}}})
        time.sleep(30)
    elif scenario == "bad_approval":
        send({"id": "approval-1", "method": "item/commandExecution/requestApproval", "params": {"threadId": "thread-1", "turnId": turn_id}})
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
    let fixture_path = temp.path().join("compatibility.json");
    fs::write(&fixture_path, COMPATIBILITY_FIXTURE).expect("copy compatibility fixture");
    let log_path = temp.path().join("fake.log");
    let command = format!(
        "export FAKE_CODEX_LOG={} FAKE_CODEX_FIXTURE={}; exec python3 {} {}",
        shell_quote_path(&log_path),
        shell_quote_path(&fixture_path),
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
    run_scenario_with_github_graphql(scenario, None).await
}

async fn run_scenario_with_github_graphql(
    scenario: &str,
    github_graphql: Option<GitHubGraphqlExecutor>,
) -> (Harness, symphony::Result<Vec<CodexEvent>>) {
    let harness = harness(scenario);
    let client =
        CodexAppServerClient::with_github_graphql(config(harness.command.clone()), github_graphql);
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

struct ToolServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ToolServer {
    fn new(status: u16, body: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tool server");
        let url = format!("http://{}/graphql", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept GraphQL request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).expect("read GraphQL request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4);
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("content length");
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            request_log
                .lock()
                .expect("requests mutex")
                .push(String::from_utf8(request).expect("utf8 request"));
            let body = body.to_string();
            let reason = if status == 200 { "OK" } else { "Bad Gateway" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write GraphQL response");
        });
        Self {
            url,
            requests,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

impl Drop for ToolServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("tool server exits");
        }
    }
}

fn github_executor(endpoint: String) -> GitHubGraphqlExecutor {
    GitHubGraphqlExecutor::from_tracker_config(&TrackerConfig {
        kind: "github".to_string(),
        endpoint,
        api_key: Some("test-token".to_string()),
        active_states: Vec::new(),
        terminal_states: Vec::new(),
        github: Some(GithubConfig {
            repository_owner: "octo-org".to_string(),
            repository_name: "octo-repo".to_string(),
            repositories: vec![GithubRepositoryConfig {
                owner: "octo-org".to_string(),
                name: "octo-repo".to_string(),
            }],
            project_owner_type: GithubProjectOwnerType::Organization,
            project_owner_login: "octo-org".to_string(),
            project_number: 1,
            status_field_name: "Status".to_string(),
            priority_field_name: None,
            blocker_field_name: None,
            blocker_label_prefix: None,
            priority_labels: Default::default(),
        }),
    })
    .expect("configured GraphQL executor")
}

fn tool_response(harness: &Harness) -> Value {
    log_entries(&harness.log_path)
        .into_iter()
        .find_map(|entry| entry.get("tool_response").cloned())
        .expect("tool response")
}

async fn assert_child_reaped(harness: &Harness) {
    let pid = log_entries(&harness.log_path)
        .iter()
        .find_map(|entry| entry.get("pid").and_then(Value::as_u64))
        .expect("fake app-server pid");
    for _ in 0..100 {
        if !process_is_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("app-server process {pid} remained alive after protocol failure");
}

fn compatibility_fixture() -> Value {
    serde_json::from_str(COMPATIBILITY_FIXTURE).expect("valid scrubbed Codex v2 fixture")
}

#[tokio::test]
async fn sends_schema_shaped_startup_messages_and_streams_completion() {
    let (harness, result) = run_scenario("complete").await;
    let events = result.expect("run completes");
    let log = log_entries(&harness.log_path);
    let fixture = compatibility_fixture();
    assert_eq!(fixture["protocol"], "codex-app-server-v2");
    let workspace_path = fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
    let bash_workspace_path = bash_path(&workspace_path);
    assert_eq!(log[0]["cwd"], bash_workspace_path.as_str());
    let received: Vec<&Value> = log
        .iter()
        .filter_map(|entry| entry.get("received"))
        .collect();
    assert_eq!(
        received[0]["method"],
        fixture["startup"]["initialize_request"]["method"]
    );
    assert_eq!(
        received[0]["params"]["clientInfo"]["name"],
        fixture["startup"]["initialize_request"]["params"]["clientInfo"]["name"]
    );
    assert_eq!(
        received[0]["params"]["capabilities"],
        fixture["startup"]["initialize_request"]["params"]["capabilities"]
    );
    assert_eq!(received[1], &fixture["startup"]["initialized_notification"]);
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
async fn accepts_string_encoded_client_response_ids() {
    let (_harness, result) = run_scenario("string_response_ids").await;
    result.expect("string-encoded response ids remain compatible");
}

#[tokio::test]
async fn auto_approves_command_and_file_requests_for_session() {
    let (harness, result) = run_scenario("approval").await;
    let events = result.expect("run completes");
    let fixture = compatibility_fixture();
    let log = log_entries(&harness.log_path);
    let command = log
        .iter()
        .find_map(|entry| entry.get("approval_response"))
        .expect("command response");
    let file = log
        .iter()
        .find_map(|entry| entry.get("file_response"))
        .expect("file response");
    assert_eq!(command["id"], 7);
    assert_eq!(
        file["id"],
        fixture["server_requests"]["file_approval"]["id"]
    );
    assert_eq!(
        command["result"],
        fixture["server_requests"]["approval_result"]
    );
    assert_eq!(
        file["result"],
        fixture["server_requests"]["approval_result"]
    );
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
    let (harness, result) = run_scenario("tool_unknown").await;
    let events = result.expect("run completes");
    let response = tool_response(&harness);
    assert_eq!(response["result"]["success"], false);
    assert_eq!(
        response["result"]["contentItems"][0]["text"],
        json!({ "error": "unsupported_tool" }).to_string()
    );
    assert!(
        events
            .iter()
            .any(|event| event.event == "unsupported_tool_call")
    );
}

fn tool_failure_code(response: &Value) -> String {
    serde_json::from_str::<Value>(
        response["result"]["contentItems"][0]["text"]
            .as_str()
            .expect("tool failure text"),
    )
    .expect("structured failure")
    .get("error")
    .and_then(Value::as_str)
    .expect("failure code")
    .to_string()
}

#[tokio::test]
async fn github_graphql_advertises_the_v2_namespace_and_returns_the_full_success_body() {
    let body = json!({ "data": { "viewer": { "login": "octo" } } });
    let server = ToolServer::new(200, body.clone());
    let (harness, result) =
        run_scenario_with_github_graphql("tool_success", Some(github_executor(server.url.clone())))
            .await;
    result.expect("tool call completes without stalling");

    let received: Vec<Value> = log_entries(&harness.log_path)
        .into_iter()
        .filter_map(|entry| entry.get("received").cloned())
        .collect();
    assert_eq!(
        received[0]["params"]["capabilities"],
        json!({ "experimentalApi": true })
    );
    assert_eq!(
        received[2]["params"]["dynamicTools"],
        json!([{
            "type": "namespace",
            "name": "github_graphql",
            "description": "Execute one GraphQL operation using configured GitHub authentication.",
            "tools": [{
                "type": "function",
                "name": "query",
                "description": "Execute one GitHub GraphQL query or mutation.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": { "type": "string", "minLength": 1 },
                        "variables": { "type": "object", "additionalProperties": true }
                    },
                    "required": ["query"]
                }
            }]
        }])
    );
    let response = tool_response(&harness);
    assert_eq!(response["result"]["success"], true);
    assert_eq!(
        serde_json::from_str::<Value>(
            response["result"]["contentItems"][0]["text"]
                .as_str()
                .expect("success body")
        )
        .expect("JSON success body"),
        body
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("authorization: Bearer test-token"));
    let request_body = requests[0]
        .split_once("\r\n\r\n")
        .expect("HTTP request body")
        .1;
    let request: Value = serde_json::from_str(request_body).expect("GraphQL request");
    assert_eq!(
        request["query"],
        "query($owner: String!) { viewer { login } }"
    );
    assert_eq!(request["variables"], json!({ "owner": "octo-org" }));
}

#[tokio::test]
async fn github_graphql_errors_preserve_the_response_body_but_fail_the_tool() {
    let body = json!({ "data": null, "errors": [{ "message": "not authorized" }] });
    let server = ToolServer::new(200, body.clone());
    let (harness, result) =
        run_scenario_with_github_graphql("tool_success", Some(github_executor(server.url.clone())))
            .await;
    result.expect("GraphQL errors do not stall the turn");
    let response = tool_response(&harness);
    assert_eq!(response["result"]["success"], false);
    assert_eq!(
        serde_json::from_str::<Value>(
            response["result"]["contentItems"][0]["text"]
                .as_str()
                .expect("GraphQL body")
        )
        .expect("JSON GraphQL body"),
        body
    );
}

#[tokio::test]
async fn github_graphql_invalid_arguments_and_missing_configuration_fail_without_stalling() {
    for scenario in ["tool_invalid", "tool_invalid_variables"] {
        let result = timeout(Duration::from_secs(1), run_scenario(scenario))
            .await
            .expect("invalid tool call must not hang");
        let (harness, result) = result;
        result.expect("invalid tool call does not fail the turn");
        assert!(
            ["invalid_query", "invalid_variables"]
                .contains(&tool_failure_code(&tool_response(&harness)).as_str())
        );
    }

    let result = timeout(Duration::from_secs(1), run_scenario("tool_success"))
        .await
        .expect("unavailable tool must not hang");
    let (harness, result) = result;
    result.expect("unavailable tool does not fail the turn");
    assert_eq!(
        tool_failure_code(&tool_response(&harness)),
        "github_graphql_unavailable"
    );
    let received: Vec<Value> = log_entries(&harness.log_path)
        .into_iter()
        .filter_map(|entry| entry.get("received").cloned())
        .collect();
    assert_eq!(received[0]["params"]["capabilities"], Value::Null);
    assert!(received[2]["params"].get("dynamicTools").is_none());
}

#[tokio::test]
async fn github_graphql_status_and_transport_failures_are_safe_and_secret_free() {
    let server = ToolServer::new(502, json!({ "message": "Bearer test-token" }));
    let (harness, result) =
        run_scenario_with_github_graphql("tool_success", Some(github_executor(server.url.clone())))
            .await;
    result.expect("HTTP status failure does not stall the turn");
    let response = tool_response(&harness);
    assert_eq!(
        tool_failure_code(&response),
        "github_graphql_request_failed"
    );
    assert!(!response.to_string().contains("test-token"));

    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve transport address");
    let endpoint = format!("http://{}/graphql", listener.local_addr().expect("address"));
    drop(listener);
    let (harness, result) =
        run_scenario_with_github_graphql("tool_success", Some(github_executor(endpoint))).await;
    result.expect("transport failure does not stall the turn");
    assert_eq!(
        tool_failure_code(&tool_response(&harness)),
        "github_graphql_request_failed"
    );
}

#[tokio::test]
async fn unknown_server_requests_receive_a_method_not_found_response() {
    let (harness, result) = run_scenario("unknown_request").await;
    result.expect("unknown request does not fail the turn");
    let log = log_entries(&harness.log_path);
    let response = log
        .iter()
        .find_map(|entry| entry.get("unknown_response"))
        .expect("unknown request response");
    assert_eq!(response["id"], "unknown-1");
    assert_eq!(response["error"]["code"], -32601);
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
    let fixture = compatibility_fixture();
    let log = log_entries(&harness.log_path);
    let input_response = log
        .iter()
        .find_map(|entry| entry.get("input_response"))
        .expect("user input response");
    assert_eq!(
        input_response["result"],
        fixture["server_requests"]["user_input_result"]
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
async fn protocol_drift_errors_are_typed_and_reap_the_child() {
    for scenario in [
        "bad_status",
        "bad_usage",
        "bad_rate_limits",
        "missing_request_id",
        "bad_approval",
    ] {
        let harness = harness(scenario);
        let client = CodexAppServerClient::new(config(harness.command.clone()));
        let workspace_path =
            fs::canonicalize(harness.workspace.path()).expect("canonical workspace");
        let mut on_event = |_| {};
        let mut session = client
            .start_session(&workspace_path, &mut on_event)
            .await
            .expect("session starts");
        let error = session
            .run_turn("scrubbed protocol test")
            .await
            .expect_err("protocol drift must fail")
            .to_string();
        assert!(error.contains("protocol_error"), "{scenario}: {error}");
        assert_child_reaped(&harness).await;
        session.shutdown().await;
    }
}

#[tokio::test]
async fn malformed_startup_responses_are_typed_and_reap_the_child() {
    for scenario in [
        "startup_missing_id",
        "startup_missing_field",
        "startup_bad_id",
        "thread_missing_id",
    ] {
        let harness = harness(scenario);
        let client = CodexAppServerClient::new(config(harness.command.clone()));
        let mut on_event = |_| {};
        let error = match client
            .start_session(harness.workspace.path(), &mut on_event)
            .await
        {
            Ok(mut session) => {
                session.shutdown().await;
                panic!("malformed startup response must fail");
            }
            Err(error) => error.to_string(),
        };
        assert!(error.contains("protocol_error"), "{scenario}: {error}");
        assert_child_reaped(&harness).await;
    }
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
