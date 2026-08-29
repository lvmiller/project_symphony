use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{Instant, timeout};

use crate::config::CodexConfig;
use crate::domain::{CodexEvent, ExecutionTarget, TokenTotals};
use crate::error::{Result, SymphonyError};
use crate::tracker::github::GitHubGraphqlExecutor;

const MAX_JSONL_MESSAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_EVENT_SUMMARY_BYTES: usize = 1024;
const METHOD_INITIALIZE: &str = "initialize";
const METHOD_INITIALIZED: &str = "initialized";
const METHOD_THREAD_START: &str = "thread/start";
const METHOD_TURN_START: &str = "turn/start";
const METHOD_TURN_COMPLETED: &str = "turn/completed";
const METHOD_TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
const METHOD_RATE_LIMITS_UPDATED: &str = "account/rateLimits/updated";
const METHOD_COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
const METHOD_FILE_APPROVAL: &str = "item/fileChange/requestApproval";
const METHOD_TOOL_CALL: &str = "item/tool/call";
const METHOD_USER_INPUT: &str = "item/tool/requestUserInput";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    client_info: ClientInfo,
    capabilities: Value,
}

#[derive(Serialize)]
struct ClientInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStartParams {
    cwd: String,
    ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dynamic_tools: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams<'a> {
    thread_id: &'a str,
    cwd: String,
    input: [TextInput<'a>; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_policy: Option<Value>,
}

#[derive(Serialize)]
struct TextInput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    codex_home: String,
    auth_mode: String,
}

#[derive(Deserialize)]
struct ThreadStartResult {
    thread: ProtocolIdentity,
}

#[derive(Deserialize)]
struct TurnStartResult {
    turn: TurnIdentity,
}

#[derive(Deserialize)]
struct ProtocolIdentity {
    id: String,
}

#[derive(Deserialize)]
struct TurnIdentity {
    id: String,
    status: String,
}

#[derive(Clone, Debug)]
enum ServerRequestId {
    Number(i64),
    String(String),
}

impl ServerRequestId {
    fn into_value(self) -> Value {
        match self {
            Self::Number(id) => json!(id),
            Self::String(id) => json!(id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub thread_id: String,
    pub turn_id: String,
    pub session_id: String,
}

#[async_trait]
pub trait CodexClient: Send + Sync {
    async fn start_session<'a>(
        &'a self,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Box<dyn CodexSession + 'a>>;

    async fn start_session_on_target<'a>(
        &'a self,
        target: &ExecutionTarget,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Box<dyn CodexSession + 'a>> {
        if target.is_local() {
            self.start_session(workspace, on_event).await
        } else {
            Err(SymphonyError::codex(
                "unsupported_execution_target",
                "Codex client does not support SSH execution targets",
            ))
        }
    }
}

#[async_trait]
pub trait CodexSession: Send {
    async fn run_turn(&mut self, prompt: &str) -> Result<TurnOutcome>;
    async fn shutdown(&mut self);
}

#[derive(Clone, Debug)]
pub struct CodexAppServerClient {
    pub config: CodexConfig,
    github_graphql: Option<GitHubGraphqlExecutor>,
}

impl CodexAppServerClient {
    pub fn new(config: CodexConfig) -> Self {
        Self::with_github_graphql(config, None)
    }

    pub fn with_github_graphql(
        config: CodexConfig,
        github_graphql: Option<GitHubGraphqlExecutor>,
    ) -> Self {
        Self {
            config,
            github_graphql,
        }
    }
}

#[async_trait]
impl CodexClient for CodexAppServerClient {
    async fn start_session<'a>(
        &'a self,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Box<dyn CodexSession + 'a>> {
        self.start_session_on_target(&ExecutionTarget::Local, workspace, on_event)
            .await
    }

    async fn start_session_on_target<'a>(
        &'a self,
        target: &ExecutionTarget,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Box<dyn CodexSession + 'a>> {
        let mut session = CodexJsonlSession::spawn(
            &self.config,
            self.github_graphql.as_ref(),
            target,
            workspace,
            on_event,
        )
        .await?;
        let startup: Result<()> = async {
            session.initialize().await?;
            let thread_id = session.start_thread().await?;
            session.thread_id = Some(thread_id);
            session.emit("thread_started", None, None, None);
            Ok(())
        }
        .await;
        match startup {
            Ok(()) => Ok(Box::new(session)),
            Err(error) => {
                session.emit(
                    "startup_failed",
                    Some(startup_failure_summary(&error)),
                    None,
                    None,
                );
                session.shutdown().await;
                Err(error)
            }
        }
    }
}

fn emit_startup_failed(on_event: &mut (dyn FnMut(CodexEvent) + Send), error: &SymphonyError) {
    on_event(CodexEvent {
        issue_id: String::new(),
        event: "startup_failed".to_string(),
        timestamp: Utc::now(),
        session_id: None,
        thread_id: None,
        turn_id: None,
        codex_app_server_pid: None,
        message: event_summary("startup_failed", Some(startup_failure_summary(error)), None),
        absolute_token_totals: None,
        rate_limits: None,
    });
}

struct CodexJsonlSession<'a> {
    config: &'a CodexConfig,
    github_graphql: Option<&'a GitHubGraphqlExecutor>,
    target: ExecutionTarget,
    workspace: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_request_id: i64,
    on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    thread_id: Option<String>,
    turn_id: Option<String>,
    session_id: Option<String>,
}

impl<'a> CodexJsonlSession<'a> {
    async fn spawn(
        config: &'a CodexConfig,
        github_graphql: Option<&'a GitHubGraphqlExecutor>,
        target: &ExecutionTarget,
        workspace: &Path,
        on_event: &'a mut (dyn FnMut(CodexEvent) + Send),
    ) -> Result<Self> {
        let mut command = match target {
            ExecutionTarget::Local => {
                let bash_command = match bash_command_in_workspace(workspace, &config.command) {
                    Ok(command) => command,
                    Err(error) => {
                        emit_startup_failed(on_event, &error);
                        return Err(error);
                    }
                };
                let mut command = Command::new("bash");
                command.arg("-lc").arg(bash_command);
                command
            }
            ExecutionTarget::Ssh { host } => {
                let script = match remote_codex_command(workspace, &config.command) {
                    Ok(script) => script,
                    Err(error) => {
                        emit_startup_failed(on_event, &error);
                        return Err(error);
                    }
                };
                let mut command = Command::new("ssh");
                command.args(["--", host, "sh", "-lc"]).arg(script);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(not(windows))]
        if target.is_local() {
            command.current_dir(workspace);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                let error = SymphonyError::io(None, source);
                emit_startup_failed(on_event, &error);
                return Err(error);
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let error = SymphonyError::codex(
                    "spawn_failed",
                    "codex app-server stdin was not available",
                );
                let _ = child.kill().await;
                let _ = child.wait().await;
                emit_startup_failed(on_event, &error);
                return Err(error);
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let error = SymphonyError::codex(
                    "spawn_failed",
                    "codex app-server stdout was not available",
                );
                let _ = child.kill().await;
                let _ = child.wait().await;
                emit_startup_failed(on_event, &error);
                return Err(error);
            }
        };
        Ok(Self {
            config,
            github_graphql,
            target: target.clone(),
            workspace: workspace.to_path_buf(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_request_id: 1,
            on_event,
            thread_id: None,
            turn_id: None,
            session_id: None,
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        let id = self
            .send_typed_request(
                METHOD_INITIALIZE,
                InitializeParams {
                    client_info: ClientInfo {
                        name: "symphony",
                        version: env!("CARGO_PKG_VERSION"),
                    },
                    capabilities: if self.github_graphql.is_some() {
                        json!({ "experimentalApi": true })
                    } else {
                        Value::Null
                    },
                },
            )
            .await?;
        let response = self.wait_for_response(id, METHOD_INITIALIZE).await?;
        parse_initialize_response(&response)?;
        self.send_notification(METHOD_INITIALIZED, Value::Null)
            .await
    }

    async fn start_thread(&mut self) -> Result<String> {
        let id = self
            .send_typed_request(
                METHOD_THREAD_START,
                ThreadStartParams {
                    cwd: self.workspace_string()?,
                    ephemeral: true,
                    approval_policy: self.config.approval_policy.clone(),
                    sandbox: self.config.thread_sandbox.clone(),
                    dynamic_tools: self
                        .github_graphql
                        .as_ref()
                        .map(|_| github_graphql_dynamic_tools()),
                },
            )
            .await?;
        let response = self.wait_for_response(id, METHOD_THREAD_START).await?;
        let result: ThreadStartResult = parse_protocol_result(METHOD_THREAD_START, &response)?;
        nonempty_protocol_id(METHOD_THREAD_START, "thread.id", result.thread.id)
    }

    async fn start_turn(&mut self, thread_id: &str, prompt: &str) -> Result<String> {
        let id = self
            .send_typed_request(
                METHOD_TURN_START,
                TurnStartParams {
                    thread_id,
                    cwd: self.workspace_string()?,
                    input: [TextInput {
                        kind: "text",
                        text: prompt,
                    }],
                    approval_policy: self.config.approval_policy.clone(),
                    sandbox_policy: self.config.turn_sandbox_policy.clone(),
                },
            )
            .await?;
        let response = self.wait_for_response(id, METHOD_TURN_START).await?;
        let result: TurnStartResult = parse_protocol_result(METHOD_TURN_START, &response)?;
        if result.turn.status != "inProgress" {
            return Err(protocol_error(format!(
                "turn/start response has unsupported turn status {}",
                result.turn.status
            )));
        }
        nonempty_protocol_id(METHOD_TURN_START, "turn.id", result.turn.id)
    }

    async fn stream_until_turn_completed(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.config.turn_timeout_ms.max(1));
        loop {
            let value = self.read_message_before(deadline).await?;
            match message_method(&value)? {
                Some(method) if value.get("id").is_some() => {
                    let id = parse_server_request_id(value.get("id"))?;
                    self.handle_server_request(id, method, &value).await?;
                }
                Some(method) if is_server_request_method(method) => {
                    return Err(protocol_error(format!(
                        "server request {method} missing id"
                    )));
                }
                Some(METHOD_TURN_COMPLETED) => return self.handle_turn_completed(&value),
                Some(METHOD_TOKEN_USAGE_UPDATED) => self.handle_token_usage(&value)?,
                Some(METHOD_RATE_LIMITS_UPDATED) => self.handle_rate_limits(&value)?,
                Some(notification) => {
                    self.emit("notification", Some(notification.to_string()), None, None);
                }
                None if value.get("id").is_some() => {
                    return Err(protocol_error(
                        "response without a method during active turn",
                    ));
                }
                None => return Err(protocol_error("message missing id or method")),
            }
        }
    }

    fn handle_turn_completed(&mut self, value: &Value) -> Result<()> {
        let params = required_object(value, "params")?;
        let turn = required_object_in_map(params, "turn")?;
        let turn_id = required_string(turn, "id")?;
        let status = required_string(turn, "status")?;
        if let Some(active_turn_id) = self.turn_id.as_deref()
            && active_turn_id != turn_id
        {
            return Err(protocol_error(format!(
                "turn/completed id {turn_id} does not match active turn {active_turn_id}"
            )));
        }
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
                Err(protocol_error(format!("unsupported turn status {other}")))
            }
        }
    }

    fn handle_token_usage(&mut self, value: &Value) -> Result<()> {
        let params = required_object(value, "params")?;
        let usage = required_object_in_map(params, "tokenUsage")?;
        let total = required_object_in_map(usage, "total")?;
        let totals = TokenTotals {
            input_tokens: required_nonnegative_i64(total, "inputTokens")?,
            output_tokens: required_nonnegative_i64(total, "outputTokens")?,
            total_tokens: required_nonnegative_i64(total, "totalTokens")?,
        };
        self.emit("token_totals", None, Some(totals), None);
        Ok(())
    }

    fn handle_rate_limits(&mut self, value: &Value) -> Result<()> {
        let params = required_object(value, "params")?;
        let rate_limits = required_object_in_map(params, "rateLimits")?;
        self.emit(
            "rate_limits",
            None,
            None,
            Some(Value::Object(rate_limits.clone())),
        );
        Ok(())
    }

    async fn wait_for_response(&mut self, id: i64, method: &str) -> Result<Value> {
        let deadline = Instant::now() + self.response_timeout();
        loop {
            let value = self.read_response_before(deadline, method, id).await?;
            match message_method(&value)? {
                Some(server_method) if value.get("id").is_some() => {
                    let request_id = parse_server_request_id(value.get("id"))?;
                    self.handle_server_request(request_id, server_method, &value)
                        .await?;
                }
                Some(method) if is_server_request_method(method) => {
                    return Err(protocol_error(format!(
                        "server request {method} missing id"
                    )));
                }
                Some(METHOD_RATE_LIMITS_UPDATED) => self.handle_rate_limits(&value)?,
                Some(METHOD_TOKEN_USAGE_UPDATED) => self.handle_token_usage(&value)?,
                Some(notification) => {
                    self.emit("notification", Some(notification.to_string()), None, None);
                }
                None => {
                    let message_id = parse_client_response_id(value.get("id"))?;
                    if message_id == id {
                        if let Some(error) = value.get("error") {
                            return Err(protocol_error(format!(
                                "codex returned error for request {id}: {error}"
                            )));
                        }
                        return value.get("result").cloned().ok_or_else(|| {
                            protocol_error(format!("response {id} missing result"))
                        });
                    }
                    self.emit("other_message", None, None, None);
                }
            }
        }
    }

    fn response_timeout(&self) -> Duration {
        let turn_timeout = Duration::from_millis(self.config.turn_timeout_ms.max(1));
        match u64::try_from(self.config.stall_timeout_ms) {
            Ok(stall_timeout_ms) if stall_timeout_ms > 0 => {
                turn_timeout.min(Duration::from_millis(stall_timeout_ms))
            }
            _ => turn_timeout,
        }
    }

    async fn read_response_before(
        &mut self,
        deadline: Instant,
        method: &str,
        id: i64,
    ) -> Result<Value> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(response_timeout_error(method, id));
            }
            let remaining = deadline.saturating_duration_since(now);
            let read_timeout = Duration::from_millis(self.config.read_timeout_ms.max(1));
            match timeout(remaining.min(read_timeout), self.read_line()).await {
                Ok(Ok(Some(line))) => return self.parse_line(&line),
                Ok(Ok(None)) => {
                    return Err(SymphonyError::codex(
                        "process_exit",
                        "codex app-server exited",
                    ));
                }
                Ok(Err(error)) => return Err(error),
                Err(_) if Instant::now() >= deadline => {
                    return Err(response_timeout_error(method, id));
                }
                Err(_) => continue,
            }
        }
    }

    async fn handle_server_request(
        &mut self,
        id: ServerRequestId,
        method: &str,
        value: &Value,
    ) -> Result<()> {
        match method {
            METHOD_COMMAND_APPROVAL | METHOD_FILE_APPROVAL => {
                validate_request_fields(value, &["itemId", "threadId", "turnId"])?;
                self.send_response(id.into_value(), json!({ "decision": "decline" }))
                    .await?;
                self.emit("approval_declined", Some(method.to_string()), None, None);
                Ok(())
            }
            METHOD_TOOL_CALL => {
                let params =
                    validate_request_fields(value, &["threadId", "turnId", "tool", "callId"])?;
                let namespace = params.get("namespace").and_then(Value::as_str);
                let tool = params.get("tool").and_then(Value::as_str);
                let result = match (namespace, tool) {
                    (Some("github_graphql"), Some("query")) => {
                        self.emit("dynamic_tool_call", None, None, None);
                        self.handle_github_graphql_tool(params).await
                    }
                    _ => {
                        self.emit("unsupported_tool_call", None, None, None);
                        github_graphql_failure("unsupported_tool")
                    }
                };
                self.send_response(id.into_value(), result).await
            }
            METHOD_USER_INPUT => {
                let params = validate_request_fields(value, &["itemId", "threadId", "turnId"])?;
                if !params.get("questions").is_some_and(Value::is_array) {
                    return Err(protocol_error(
                        "item/tool/requestUserInput request missing array questions",
                    ));
                }
                self.send_response(id.into_value(), json!({ "answers": {} }))
                    .await?;
                self.emit(
                    "turn_input_required",
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
                self.send_error(id.into_value(), -32601, "unsupported server request")
                    .await?;
                self.emit("other_message", Some(method.to_string()), None, None);
                Ok(())
            }
        }
    }
    async fn handle_github_graphql_tool(
        &mut self,
        params: &serde_json::Map<String, Value>,
    ) -> Value {
        let Some(arguments) = params.get("arguments").and_then(Value::as_object) else {
            return github_graphql_failure("invalid_arguments");
        };
        let Some(query) = arguments.get("query").and_then(Value::as_str) else {
            return github_graphql_failure("invalid_query");
        };
        if query.trim().is_empty() || !is_single_graphql_operation(query) {
            return github_graphql_failure("invalid_query");
        }
        let variables = match arguments.get("variables") {
            Some(variables) if variables.is_object() => variables.clone(),
            Some(_) => return github_graphql_failure("invalid_variables"),
            None => json!({}),
        };
        let Some(executor) = self.github_graphql else {
            return github_graphql_failure("github_graphql_unavailable");
        };

        match timeout(self.response_timeout(), executor.execute(query, variables)).await {
            Ok(Ok(body)) => github_graphql_response(body),
            Ok(Err(_)) => github_graphql_failure("github_graphql_request_failed"),
            Err(_) => github_graphql_failure("github_graphql_request_timed_out"),
        }
    }

    async fn send_typed_request<T: Serialize>(
        &mut self,
        method: &'static str,
        params: T,
    ) -> Result<i64> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.write_json(&json!({
            "id": id,
            "method": method,
            "params": serde_json::to_value(params)?
        }))
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
            match timeout(wait, self.read_line()).await {
                Ok(Ok(Some(line))) => return self.parse_line(&line),
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
                Ok(Err(error)) => return Err(error),
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

    async fn read_line(&mut self) -> Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        loop {
            let mut consumed = 0;
            let mut complete = false;
            let mut too_large = false;
            {
                let buffer = self
                    .stdout
                    .fill_buf()
                    .await
                    .map_err(|source| SymphonyError::io(None, source))?;
                if buffer.is_empty() {
                    return if line.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(line))
                    };
                }
                let payload_len = buffer
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap_or(buffer.len());
                if line.len() + payload_len > MAX_JSONL_MESSAGE_BYTES {
                    too_large = true;
                } else {
                    line.extend_from_slice(&buffer[..payload_len]);
                    complete = payload_len < buffer.len();
                    consumed = payload_len + usize::from(complete);
                }
            }
            if too_large {
                self.emit(
                    "malformed",
                    Some(format!(
                        "jsonl message exceeded {MAX_JSONL_MESSAGE_BYTES} byte limit"
                    )),
                    None,
                    None,
                );
                return Err(SymphonyError::codex(
                    "protocol_error",
                    format!("jsonl message exceeded {MAX_JSONL_MESSAGE_BYTES} byte limit"),
                ));
            }
            self.stdout.consume(consumed);
            if complete {
                return Ok(Some(line));
            }
        }
    }

    fn parse_line(&mut self, line: &[u8]) -> Result<Value> {
        match serde_json::from_slice::<Value>(line) {
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
            message: event_summary(event, message, absolute_token_totals.as_ref()),
            absolute_token_totals,
            rate_limits,
        });
    }

    fn workspace_string(&self) -> Result<String> {
        match &self.target {
            ExecutionTarget::Local => bash_path(&self.workspace),
            ExecutionTarget::Ssh { .. } => {
                self.workspace.to_str().map(str::to_owned).ok_or_else(|| {
                    SymphonyError::codex(
                        "invalid_workspace_cwd",
                        "remote Codex workspace path is not valid UTF-8",
                    )
                })
            }
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.stdin.shutdown().await;
        if timeout(Duration::from_millis(200), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}

#[async_trait]
impl CodexSession for CodexJsonlSession<'_> {
    async fn run_turn(&mut self, prompt: &str) -> Result<TurnOutcome> {
        let result = async {
            let thread_id = self
                .thread_id
                .clone()
                .ok_or_else(|| protocol_error("session started without a thread id"))?;
            let turn_id = self.start_turn(&thread_id, prompt).await?;
            let session_id = compose_session_id(&thread_id, &turn_id);
            self.turn_id = Some(turn_id.clone());
            self.session_id = Some(session_id.clone());
            self.emit("session_started", None, None, None);
            self.emit("turn_started", None, None, None);
            self.stream_until_turn_completed().await?;
            Ok(TurnOutcome {
                thread_id,
                turn_id,
                session_id,
            })
        }
        .await;
        if is_protocol_error(&result) {
            self.emit("protocol_error", None, None, None);
            self.shutdown().await;
        }
        result
    }

    async fn shutdown(&mut self) {
        CodexJsonlSession::shutdown(self).await;
    }
}

fn github_graphql_dynamic_tools() -> Value {
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
                    "variables": {
                        "type": "object",
                        "additionalProperties": true
                    }
                },
                "required": ["query"]
            }
        }]
    }])
}

fn github_graphql_response(body: Value) -> Value {
    let success = !body
        .get("errors")
        .is_some_and(|errors| !errors.is_null() && !errors.as_array().is_some_and(Vec::is_empty));
    json!({
        "success": success,
        "contentItems": [{
            "type": "inputText",
            "text": body.to_string()
        }]
    })
}

fn github_graphql_failure(code: &str) -> Value {
    json!({
        "success": false,
        "contentItems": [{
            "type": "inputText",
            "text": json!({ "error": code }).to_string()
        }]
    })
}

fn is_single_graphql_operation(query: &str) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Definition {
        None,
        Operation,
        Fragment,
    }

    let bytes = query.as_bytes();
    let mut index = 0;
    let mut brace_depth = 0;
    let mut operations = 0;
    let mut definition = Definition::None;
    while index < bytes.len() {
        match bytes[index] {
            b'#' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                    index += 1;
                }
            }
            b'"' if bytes[index..].starts_with(b"\"\"\"") => {
                index += 3;
                while index < bytes.len() && !bytes[index..].starts_with(b"\"\"\"") {
                    index += 1;
                }
                if index == bytes.len() {
                    return false;
                }
                index += 3;
            }
            b'"' => {
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b'"' && !escaped {
                        break;
                    }
                    escaped = byte == b'\\' && !escaped;
                    if byte != b'\\' {
                        escaped = false;
                    }
                }
                if index == bytes.len() && bytes.last() != Some(&b'"') {
                    return false;
                }
            }
            b'{' => {
                if brace_depth == 0 && definition == Definition::None {
                    operations += 1;
                    definition = Definition::Operation;
                }
                brace_depth += 1;
                index += 1;
            }
            b'}' => {
                if brace_depth == 0 {
                    return false;
                }
                brace_depth -= 1;
                if brace_depth == 0 {
                    definition = Definition::None;
                }
                index += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                if brace_depth == 0 {
                    match &query[start..index] {
                        "query" | "mutation" | "subscription" => {
                            operations += 1;
                            definition = Definition::Operation;
                        }
                        "fragment" => definition = Definition::Fragment,
                        _ => {}
                    }
                }
            }
            _ => index += 1,
        }
    }
    brace_depth == 0 && operations == 1
}

fn bash_command_in_workspace(workspace: &Path, command: &str) -> Result<String> {
    #[cfg(windows)]
    {
        Ok(format!(
            "cd -- {} || exit $?; {command}",
            shell_quote(&bash_path(workspace)?)
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = workspace;
        Ok(command.to_string())
    }
}

fn remote_codex_command(workspace: &Path, command: &str) -> Result<String> {
    let workspace = workspace.to_str().ok_or_else(|| {
        SymphonyError::codex(
            "invalid_workspace_cwd",
            "remote Codex workspace path is not valid UTF-8",
        )
    })?;
    if workspace.is_empty() {
        return Err(SymphonyError::codex(
            "invalid_workspace_cwd",
            "remote Codex workspace path is empty",
        ));
    }
    Ok(format!(
        "cd -- {} && exec bash -lc {}",
        shell_quote(workspace),
        shell_quote(command)
    ))
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn bash_path(path: &Path) -> Result<String> {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    if path.starts_with("UNC/") {
        return Err(SymphonyError::codex(
            "unsupported_workspace_path",
            "WSL Codex workspaces must use a local drive, not a UNC path",
        ));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/' {
        return Ok(format!(
            "/mnt/{}/{}",
            char::from(bytes[0]).to_ascii_lowercase(),
            &path[3..]
        ));
    }
    Err(SymphonyError::codex(
        "unsupported_workspace_path",
        format!("WSL Codex workspace path is not a local Windows drive: {path}"),
    ))
}

#[cfg(not(windows))]
fn bash_path(path: &Path) -> Result<String> {
    Ok(path.to_string_lossy().into_owned())
}

fn protocol_error(message: impl Into<String>) -> SymphonyError {
    SymphonyError::codex("protocol_error", message)
}

fn is_protocol_error<T>(result: &Result<T>) -> bool {
    matches!(
        result,
        Err(SymphonyError::Codex {
            kind: "protocol_error",
            ..
        })
    )
}

fn message_method(value: &Value) -> Result<Option<&str>> {
    match value.get("method") {
        None => Ok(None),
        Some(Value::String(method)) if !method.is_empty() => Ok(Some(method)),
        Some(_) => Err(protocol_error("message method must be a non-empty string")),
    }
}

fn is_server_request_method(method: &str) -> bool {
    matches!(
        method,
        METHOD_COMMAND_APPROVAL | METHOD_FILE_APPROVAL | METHOD_TOOL_CALL | METHOD_USER_INPUT
    )
}

fn parse_server_request_id(value: Option<&Value>) -> Result<ServerRequestId> {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .map(ServerRequestId::Number)
            .ok_or_else(|| protocol_error("server request id must be an integer")),
        Some(Value::String(id)) if !id.is_empty() => Ok(ServerRequestId::String(id.clone())),
        Some(Value::String(_)) => Err(protocol_error("server request id must not be empty")),
        Some(_) => Err(protocol_error(
            "server request id must be an integer or non-empty string",
        )),
        None => Err(protocol_error("server request missing id")),
    }
}

fn parse_client_response_id(value: Option<&Value>) -> Result<i64> {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| protocol_error("response id must be an integer")),
        Some(Value::String(id)) => id
            .parse::<i64>()
            .map_err(|_| protocol_error("response string id must be an integer")),
        Some(_) => Err(protocol_error(
            "response id must be an integer or string-encoded integer",
        )),
        None => Err(protocol_error("response missing id")),
    }
}

fn parse_initialize_response(value: &Value) -> Result<()> {
    let result: InitializeResult = parse_protocol_result(METHOD_INITIALIZE, value)?;
    nonempty_protocol_id(METHOD_INITIALIZE, "codexHome", result.codex_home)?;
    nonempty_protocol_id(METHOD_INITIALIZE, "authMode", result.auth_mode)?;
    Ok(())
}

fn parse_protocol_result<T: DeserializeOwned>(method: &str, value: &Value) -> Result<T> {
    if !value.is_object() {
        return Err(protocol_error(format!(
            "{method} response result must be an object"
        )));
    }
    serde_json::from_value(value.clone()).map_err(|error| {
        protocol_error(format!("{method} response has incompatible shape: {error}"))
    })
}

fn nonempty_protocol_id(method: &str, field: &str, value: String) -> Result<String> {
    if value.is_empty() {
        Err(protocol_error(format!("{method} response missing {field}")))
    } else {
        Ok(value)
    }
}

fn required_object<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| protocol_error(format!("message missing object {field}")))
}

fn required_object_in_map<'a>(
    value: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| protocol_error(format!("message missing object {field}")))
}

fn required_string<'a>(value: &'a serde_json::Map<String, Value>, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| protocol_error(format!("message missing non-empty string {field}")))
}

fn required_nonnegative_i64(value: &serde_json::Map<String, Value>, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .filter(|number| *number >= 0)
        .ok_or_else(|| protocol_error(format!("message missing nonnegative integer {field}")))
}

fn validate_request_fields<'a>(
    value: &'a Value,
    fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>> {
    let params = required_object(value, "params")?;
    for field in fields {
        required_string(params, field)?;
    }
    Ok(params)
}

fn response_timeout_error(method: &str, id: i64) -> SymphonyError {
    SymphonyError::codex(
        "timeout",
        format!("timed out waiting for {method} response {id}"),
    )
}

fn turn_error_message(turn: &serde_json::Map<String, Value>) -> Option<String> {
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

fn startup_failure_summary(error: &SymphonyError) -> String {
    let SymphonyError::Codex { kind, message } = error else {
        return "Codex session startup failed".to_string();
    };
    if *kind == "timeout" && message.contains("initialize response") {
        "Codex session startup failed waiting for initialize response".to_string()
    } else if *kind == "timeout" && message.contains("thread/start response") {
        "Codex session startup failed waiting for thread/start response".to_string()
    } else {
        "Codex session startup failed".to_string()
    }
}
fn event_summary(
    event: &str,
    detail: Option<String>,
    absolute_token_totals: Option<&TokenTotals>,
) -> Option<String> {
    let summary = match event {
        "startup_failed" => detail.or_else(|| Some("Codex session startup failed".to_string())),
        "thread_started" => Some("Codex thread started".to_string()),
        "session_started" => Some("Codex session started".to_string()),
        "turn_started" => Some("Codex turn started".to_string()),
        "turn_completed" => Some("Codex turn completed".to_string()),
        "turn_failed" => Some("Codex turn failed".to_string()),
        "turn_cancelled" => Some("Codex turn interrupted".to_string()),
        "token_totals" => absolute_token_totals.map(|totals| {
            format!(
                "token totals updated: input={}, output={}, total={}",
                totals.input_tokens, totals.output_tokens, totals.total_tokens
            )
        }),
        "rate_limits" => Some("Codex rate limits updated".to_string()),
        "approval_declined" => match detail.as_deref() {
            Some(METHOD_COMMAND_APPROVAL) => Some("command approval declined".to_string()),
            Some(METHOD_FILE_APPROVAL) => Some("file-change approval declined".to_string()),
            _ => Some("approval declined".to_string()),
        },
        "turn_input_required" => Some("Codex requires user input".to_string()),
        "unsupported_tool_call" => Some("unsupported dynamic tool request".to_string()),
        "dynamic_tool_call" => Some("GitHub GraphQL dynamic tool request".to_string()),
        "notification" => detail.map(|method| {
            format!(
                "Codex notification: {}",
                protocol_method_summary(method.as_str())
            )
        }),
        "other_message" => detail.map(|method| {
            format!(
                "unsupported Codex server request: {}",
                protocol_method_summary(method.as_str())
            )
        }),
        "malformed" => Some("malformed Codex protocol message".to_string()),
        "protocol_error" => Some("Codex protocol error".to_string()),
        _ => None,
    };
    summary.map(truncate_event_summary)
}

fn protocol_method_summary(method: &str) -> String {
    let method = method.trim();
    if method.is_empty() {
        return "unknown".to_string();
    }
    let method_lowercase = method.to_ascii_lowercase();
    if [
        "authorization",
        "api_key",
        "credential",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|label| method_lowercase.contains(label))
    {
        "[redacted]".to_string()
    } else {
        method.to_string()
    }
}

fn truncate_event_summary(summary: String) -> String {
    if summary.len() <= MAX_EVENT_SUMMARY_BYTES {
        return summary;
    }

    let content_limit = MAX_EVENT_SUMMARY_BYTES - '…'.len_utf8();
    let mut end = content_limit;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &summary[..end])
}
