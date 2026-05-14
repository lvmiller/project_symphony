use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{Instant, timeout};

use crate::config::CodexConfig;
use crate::domain::{CodexEvent, TokenTotals};
use crate::error::{Result, SymphonyError};

#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub thread_id: String,
    pub turn_id: String,
}

#[async_trait]
pub trait CodexClient: Send + Sync {
    async fn run_turn(
        &self,
        workspace: &Path,
        prompt: &str,
        on_event: &mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<TurnOutcome>;
}

#[derive(Clone, Debug)]
pub struct CodexAppServerClient {
    pub config: CodexConfig,
}

impl CodexAppServerClient {
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl CodexClient for CodexAppServerClient {
    async fn run_turn(
        &self,
        workspace: &Path,
        prompt: &str,
        on_event: &mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<TurnOutcome> {
        let mut session = CodexJsonlSession::spawn(&self.config, workspace, on_event).await?;
        let result = async {
            session.initialize().await?;
            let thread_id = session.start_thread().await?;
            let turn_id = session.start_turn(&thread_id, prompt).await?;
            let session_id = compose_session_id(&thread_id, &turn_id);
            session.thread_id = Some(thread_id.clone());
            session.turn_id = Some(turn_id.clone());
            session.session_id = Some(session_id);
            session.emit("session_started", None, None, None);
            session.emit("turn_started", None, None, None);
            session.stream_until_turn_completed().await?;
            Ok(TurnOutcome { thread_id, turn_id })
        }
        .await;
        session.shutdown().await;
        result
    }
}

struct CodexJsonlSession<'a> {
    config: &'a CodexConfig,
    workspace: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_request_id: i64,
    on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    thread_id: Option<String>,
    turn_id: Option<String>,
    session_id: Option<String>,
}

impl<'a> CodexJsonlSession<'a> {
    async fn spawn(
        config: &'a CodexConfig,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Self> {
        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(&config.command)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| SymphonyError::io(None, source))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            SymphonyError::codex("spawn_failed", "codex app-server stdin was not available")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SymphonyError::codex("spawn_failed", "codex app-server stdout was not available")
        })?;
        Ok(Self {
            config,
            workspace: workspace.to_path_buf(),
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_request_id: 1,
            on_event,
            thread_id: None,
            turn_id: None,
            session_id: None,
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        let id = self
            .send_request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "symphony",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": null
                }),
            )
            .await?;
        self.wait_for_response(id).await?;
        self.send_notification("initialized", Value::Null).await
    }

    async fn start_thread(&mut self) -> Result<String> {
        let mut params = json!({
            "cwd": self.workspace_string(),
            "ephemeral": true
        });
        insert_configured(
            &mut params,
            "approvalPolicy",
            self.config.approval_policy.as_ref(),
        );
        insert_configured(&mut params, "sandbox", self.config.thread_sandbox.as_ref());
        let id = self.send_request("thread/start", params).await?;
        let response = self.wait_for_response(id).await?;
        response
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                SymphonyError::codex("protocol_error", "thread/start response missing thread.id")
            })
    }

    async fn start_turn(&mut self, thread_id: &str, prompt: &str) -> Result<String> {
        let mut params = json!({
            "threadId": thread_id,
            "cwd": self.workspace_string(),
            "input": [{ "type": "text", "text": prompt }]
        });
        insert_configured(
            &mut params,
            "approvalPolicy",
            self.config.approval_policy.as_ref(),
        );
        insert_configured(
            &mut params,
            "sandboxPolicy",
            self.config.turn_sandbox_policy.as_ref(),
        );
        let id = self.send_request("turn/start", params).await?;
        let response = self.wait_for_response(id).await?;
        response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                SymphonyError::codex("protocol_error", "turn/start response missing turn.id")
            })
    }

    async fn stream_until_turn_completed(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.config.turn_timeout_ms.max(1));
        loop {
            let value = self.read_message_before(deadline).await?;
            if let Some(id) = value.get("id").cloned() {
                if value.get("method").and_then(Value::as_str).is_some() {
                    self.handle_server_request(id, &value).await?;
                }
                continue;
            }
            let Some(method) = value.get("method").and_then(Value::as_str) else {
                self.emit(
                    "malformed",
                    Some("message missing method".to_string()),
                    None,
                    None,
                );
                return Err(SymphonyError::codex(
                    "protocol_error",
                    "message missing method",
                ));
            };
            match method {
                "turn/completed" => return self.handle_turn_completed(&value),
                "thread/tokenUsage/updated" => self.handle_token_usage(&value),
                "account/rateLimits/updated" => self.handle_rate_limits(&value),
                _ => self.emit("notification", Some(method.to_string()), None, None),
            }
        }
    }

    fn handle_turn_completed(&mut self, value: &Value) -> Result<()> {
        let turn = value
            .get("params")
            .and_then(|params| params.get("turn"))
            .ok_or_else(|| {
                SymphonyError::codex("protocol_error", "turn/completed missing params.turn")
            })?;
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match status {
            "completed" => {
                self.emit("turn_completed", None, None, None);
                Ok(())
            }
            "failed" => {
                let message = turn_error_message(turn).unwrap_or_else(|| "turn failed".to_string());
                self.emit("turn_failed", Some(message.clone()), None, None);
                Err(SymphonyError::codex("turn_failed", message))
            }
            "interrupted" => {
                self.emit("turn_cancelled", None, None, None);
                Err(SymphonyError::codex("turn_cancelled", "turn interrupted"))
            }
            other => {
                self.emit(
                    "turn_failed",
                    Some(format!("unsupported turn status {other}")),
                    None,
                    None,
                );
                Err(SymphonyError::codex(
                    "protocol_error",
                    format!("unsupported turn status {other}"),
                ))
            }
        }
    }

    fn handle_token_usage(&mut self, value: &Value) {
        let total = value
            .get("params")
            .and_then(|params| params.get("tokenUsage"))
            .and_then(|usage| usage.get("total"));
        if let Some(total) = total {
            let totals = TokenTotals {
                input_tokens: total
                    .get("inputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                output_tokens: total
                    .get("outputTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                total_tokens: total
                    .get("totalTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            };
            self.emit("token_totals", None, Some(totals), None);
        } else {
            self.emit(
                "notification",
                Some("thread/tokenUsage/updated".to_string()),
                None,
                None,
            );
        }
    }

    fn handle_rate_limits(&mut self, value: &Value) {
        let rate_limits = value
            .get("params")
            .and_then(|params| params.get("rateLimits"))
            .cloned();
        self.emit("rate_limits", None, None, rate_limits);
    }

    async fn wait_for_response(&mut self, id: i64) -> Result<Value> {
        loop {
            let value = self
                .read_message_with_timeout(self.config.read_timeout_ms)
                .await?;
            if let Some(message_id) = request_id_as_i64(value.get("id")) {
                if value.get("method").and_then(Value::as_str).is_some() {
                    self.handle_server_request(
                        value.get("id").cloned().unwrap_or(Value::Null),
                        &value,
                    )
                    .await?;
                    continue;
                }
                if message_id == id {
                    if let Some(error) = value.get("error") {
                        return Err(SymphonyError::codex(
                            "protocol_error",
                            format!("codex returned error for request {id}: {error}"),
                        ));
                    }
                    return value.get("result").cloned().ok_or_else(|| {
                        SymphonyError::codex(
                            "protocol_error",
                            format!("response {id} missing result"),
                        )
                    });
                }
            } else if value.get("method").and_then(Value::as_str).is_some() {
                self.handle_notification_before_turn(&value);
            } else {
                self.emit(
                    "malformed",
                    Some("message missing id or method".to_string()),
                    None,
                    None,
                );
                return Err(SymphonyError::codex(
                    "protocol_error",
                    "message missing id or method",
                ));
            }
        }
    }

    fn handle_notification_before_turn(&mut self, value: &Value) {
        if value.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated") {
            self.handle_rate_limits(value);
        } else if value.get("method").and_then(Value::as_str) == Some("thread/tokenUsage/updated") {
            self.handle_token_usage(value);
        }
    }

    async fn handle_server_request(&mut self, id: Value, value: &Value) -> Result<()> {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "item/commandExecution/requestApproval" => {
                self.send_response(id, json!({ "decision": "acceptForSession" }))
                    .await
            }
            "item/fileChange/requestApproval" => {
                self.send_response(id, json!({ "decision": "acceptForSession" }))
                    .await
            }
            "item/tool/call" => {
                self.send_response(id, json!({ "success": false, "contentItems": [] }))
                    .await
            }
            "item/tool/requestUserInput" => {
                self.send_response(id, json!({ "answers": {} })).await?;
                self.emit(
                    "turn_failed",
                    Some("codex requested user input".to_string()),
                    None,
                    None,
                );
                Err(SymphonyError::codex(
                    "user_input_required",
                    "codex requested user input",
                ))
            }
            _ => {
                self.send_error(id, -32601, "unsupported server request")
                    .await
            }
        }
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.write_json(&json!({ "id": id, "method": method, "params": params }))
            .await?;
        Ok(id)
    }

    async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        if params.is_null() {
            self.write_json(&json!({ "method": method })).await
        } else {
            self.write_json(&json!({ "method": method, "params": params }))
                .await
        }
    }

    async fn send_response(&mut self, id: Value, result: Value) -> Result<()> {
        self.write_json(&json!({ "id": id, "result": result }))
            .await
    }

    async fn send_error(&mut self, id: Value, code: i64, message: &str) -> Result<()> {
        self.write_json(&json!({ "id": id, "error": { "code": code, "message": message } }))
            .await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|source| SymphonyError::io(None, source))?;
        self.stdin
            .flush()
            .await
            .map_err(|source| SymphonyError::io(None, source))
    }

    async fn read_message_with_timeout(&mut self, timeout_ms: u64) -> Result<Value> {
        let timeout_ms = timeout_ms.max(1);
        match timeout(Duration::from_millis(timeout_ms), self.stdout.next_line()).await {
            Ok(Ok(Some(line))) => self.parse_line(line),
            Ok(Ok(None)) => Err(SymphonyError::codex(
                "process_exit",
                "codex app-server exited",
            )),
            Ok(Err(source)) => Err(SymphonyError::io(None, source)),
            Err(_) => Err(SymphonyError::codex(
                "timeout",
                "timed out waiting for codex response",
            )),
        }
    }

    async fn read_message_before(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.emit(
                    "turn_failed",
                    Some("codex turn timed out".to_string()),
                    None,
                    None,
                );
                return Err(SymphonyError::codex("timeout", "codex turn timed out"));
            }
            let remaining = deadline.saturating_duration_since(now);
            let read_timeout = Duration::from_millis(self.config.read_timeout_ms.max(1));
            let wait = remaining.min(read_timeout);
            match timeout(wait, self.stdout.next_line()).await {
                Ok(Ok(Some(line))) => return self.parse_line(line),
                Ok(Ok(None)) => {
                    self.emit(
                        "turn_failed",
                        Some("codex app-server exited".to_string()),
                        None,
                        None,
                    );
                    return Err(SymphonyError::codex(
                        "process_exit",
                        "codex app-server exited",
                    ));
                }
                Ok(Err(source)) => return Err(SymphonyError::io(None, source)),
                Err(_) if Instant::now() >= deadline => {
                    self.emit(
                        "turn_failed",
                        Some("codex turn timed out".to_string()),
                        None,
                        None,
                    );
                    return Err(SymphonyError::codex("timeout", "codex turn timed out"));
                }
                Err(_) => continue,
            }
        }
    }

    fn parse_line(&mut self, line: String) -> Result<Value> {
        match serde_json::from_str::<Value>(&line) {
            Ok(value) if value.is_object() => Ok(value),
            Ok(_) => {
                self.emit(
                    "malformed",
                    Some("jsonl message was not an object".to_string()),
                    None,
                    None,
                );
                Err(SymphonyError::codex(
                    "protocol_error",
                    "jsonl message was not an object",
                ))
            }
            Err(error) => {
                self.emit("malformed", Some(error.to_string()), None, None);
                Err(SymphonyError::codex(
                    "protocol_error",
                    format!("malformed jsonl message: {error}"),
                ))
            }
        }
    }

    fn emit(
        &mut self,
        event: &str,
        message: Option<String>,
        absolute_token_totals: Option<TokenTotals>,
        rate_limits: Option<Value>,
    ) {
        (self.on_event)(CodexEvent {
            issue_id: String::new(),
            event: event.to_string(),
            timestamp: Utc::now(),
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            codex_app_server_pid: self.child.id(),
            message,
            absolute_token_totals,
            rate_limits,
        });
    }

    fn workspace_string(&self) -> String {
        self.workspace.to_string_lossy().into_owned()
    }

    async fn shutdown(&mut self) {
        let _ = self.stdin.shutdown().await;
        match timeout(Duration::from_millis(200), self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.kill().await;
            }
        }
    }
}

fn insert_configured(params: &mut Value, key: &'static str, value: Option<&Value>) {
    if let (Some(object), Some(value)) = (params.as_object_mut(), value) {
        object.insert(key.to_string(), value.clone());
    }
}

fn request_id_as_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn turn_error_message(turn: &Value) -> Option<String> {
    turn.get("error")
        .and_then(|error| {
            error
                .get("message")
                .or_else(|| error.get("description"))
                .or(Some(error))
        })
        .and_then(|message| match message {
            Value::String(text) => Some(text.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
}

fn compose_session_id(thread_id: &str, turn_id: &str) -> String {
    format!("{thread_id}-{turn_id}")
}
