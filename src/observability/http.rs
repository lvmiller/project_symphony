//! Local HTTP observability and operator dashboard.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{EffectiveConfig, GithubProjectOwnerType, GithubRepositoryConfig};
use crate::domain::{RetrySnapshot, RunningSnapshot, RuntimeSnapshot, TokenTotals};
use crate::error::{Result, SymphonyError};
use crate::orchestrator::OrchestratorState;
use crate::time::{now_utc, system_monotonic_ms};
use crate::workflow::load_workflow;

#[derive(Clone, Debug)]
pub struct SharedStatus {
    inner: Arc<RwLock<StatusDocument>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusDocument {
    pub generated_at: DateTime<Utc>,
    pub state: RuntimeSnapshot,
    pub sources: Vec<SourceSummary>,
    pub issues: Vec<IssueDetail>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceSummary {
    pub source_id: String,
    pub workflow_path: String,
    pub repositories: Vec<GithubRepositoryConfig>,
    pub project_owner_type: Option<GithubProjectOwnerType>,
    pub project_owner_login: Option<String>,
    pub project_number: Option<i64>,
    pub status_field_name: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub polling_interval_ms: u64,
    pub workspace_root: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueDetail {
    pub source_id: String,
    pub issue_identifier: String,
    pub issue_id: String,
    pub status: String,
    pub workspace: WorkspaceDetail,
    pub attempts: AttemptDetail,
    pub running: Option<RunningDetail>,
    pub retry: Option<RetryDetail>,
    pub logs: LogsDetail,
    pub recent_events: Vec<RecentEvent>,
    pub last_error: Option<String>,
    pub tracked: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceDetail {
    pub path: String,
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptDetail {
    pub restart_count: u32,
    pub current_retry_attempt: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunningDetail {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
    pub turn_count: u32,
    pub state: String,
    pub started_at: DateTime<Utc>,
    pub last_event: String,
    pub last_message: String,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenTotals,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryDetail {
    pub source_id: String,
    pub issue_id: String,
    pub issue_identifier: String,
    pub workspace_key: String,
    pub attempt: u32,
    pub remaining_delay_ms: u64,
    pub error: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LogsDetail {
    pub codex_session_logs: Vec<CodexSessionLog>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodexSessionLog {
    pub label: String,
    pub path: String,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentEvent {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: Option<String>,
}

#[derive(Debug)]
pub struct HttpServerHandle {
    pub local_addr: SocketAddr,
    pub task: JoinHandle<()>,
}

#[derive(Clone)]
struct AppState {
    shared_status: SharedStatus,
    refresh_tx: mpsc::UnboundedSender<()>,
    refresh_pending: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StateResponse {
    pub generated_at: DateTime<Utc>,
    pub counts: StateResponseCounts,
    pub running: Vec<StateRunningDetail>,
    pub retrying: Vec<RetryDetail>,
    pub codex_totals: CodexTotals,
    pub rate_limits: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StateRunningDetail {
    pub source_id: String,
    pub issue_id: String,
    pub issue_identifier: String,
    pub workspace_key: String,
    pub state: String,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
    pub turn_count: u32,
    pub retry_attempt: Option<u32>,
    pub cancel_requested: bool,
    pub last_event: String,
    pub last_message: String,
    pub started_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenTotals,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub seconds_running: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StateResponseCounts {
    pub running: usize,
    pub retrying: usize,
    pub sources: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourcesResponse {
    pub generated_at: DateTime<Utc>,
    pub sources: Vec<SourceSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepositoryListResponse {
    pub source_id: String,
    pub workflow_path: String,
    pub repositories: Vec<GithubRepositoryConfig>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefreshResponse {
    pub queued: bool,
    pub coalesced: bool,
    pub requested_at: DateTime<Utc>,
    pub operations: [&'static str; 2],
}

#[derive(Clone, Debug, Deserialize)]
struct SourceQuery {
    source_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Clone, Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl SharedStatus {
    pub fn new(configs: &[EffectiveConfig]) -> Self {
        Self {
            inner: Arc::new(RwLock::new(StatusDocument {
                generated_at: now_utc(),
                state: RuntimeSnapshot::default(),
                sources: source_summaries(configs),
                issues: Vec::new(),
            })),
        }
    }

    pub async fn publish(&self, state: &OrchestratorState, configs: &[EffectiveConfig]) {
        let generated_at = now_utc();
        let observed_monotonic_ms = system_monotonic_ms();
        let snapshot = state.snapshot_at(generated_at, observed_monotonic_ms);
        let sources = source_summaries(configs);
        let issues = issue_details(&snapshot, configs);
        let mut document = self.inner.write().await;
        *document = StatusDocument {
            generated_at,
            state: snapshot,
            sources,
            issues,
        };
    }

    pub async fn snapshot(&self) -> StatusDocument {
        self.inner.read().await.clone()
    }
}

pub async fn spawn_http_server(
    bind_addr: SocketAddr,
    shared_status: SharedStatus,
    refresh_tx: mpsc::UnboundedSender<()>,
    refresh_pending: Arc<AtomicBool>,
) -> Result<HttpServerHandle> {
    let listener = TcpListener::bind(bind_addr).await.map_err(|err| {
        SymphonyError::config(
            "http_bind_failed",
            format!("failed to bind {bind_addr}: {err}"),
        )
    })?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| SymphonyError::config("http_bind_failed", err.to_string()))?;
    let app = router(shared_status, refresh_tx, refresh_pending);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            warn!(error = %error, "http_server_failed");
        }
    });
    Ok(HttpServerHandle { local_addr, task })
}

pub fn router(
    shared_status: SharedStatus,
    refresh_tx: mpsc::UnboundedSender<()>,
    refresh_pending: Arc<AtomicBool>,
) -> Router {
    let state = AppState {
        shared_status,
        refresh_tx,
        refresh_pending,
    };
    let api = Router::new()
        .route("/state", get(state_api).fallback(api_method_not_allowed))
        .route(
            "/sources",
            get(sources_api).fallback(api_method_not_allowed),
        )
        .route(
            "/repositories",
            get(repositories_api).fallback(api_method_not_allowed),
        )
        .route(
            "/refresh",
            post(refresh_api).fallback(api_method_not_allowed),
        )
        .route(
            "/{issue_identifier}",
            get(issue_detail_api).fallback(api_method_not_allowed),
        )
        .fallback(api_not_found);
    Router::new()
        .route("/", get(dashboard))
        .nest("/api/v1", api)
        .with_state(state)
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn state_api(State(state): State<AppState>) -> Json<StateResponse> {
    let document = state.shared_status.snapshot().await;
    let running = document
        .issues
        .iter()
        .filter_map(|issue| {
            issue.running.as_ref().map(|running| StateRunningDetail {
                source_id: issue.source_id.clone(),
                issue_id: issue.issue_id.clone(),
                issue_identifier: issue.issue_identifier.clone(),
                workspace_key: issue.workspace.key.clone(),
                state: running.state.clone(),
                session_id: running.session_id.clone(),
                thread_id: running.thread_id.clone(),
                turn_id: running.turn_id.clone(),
                codex_app_server_pid: running.codex_app_server_pid,
                turn_count: running.turn_count,
                retry_attempt: issue.attempts.current_retry_attempt,
                cancel_requested: issue.status == "cancel_requested",
                last_event: running.last_event.clone(),
                last_message: running.last_message.clone(),
                started_at: running.started_at,
                last_event_at: running.last_event_at,
                tokens: running.tokens.clone(),
            })
        })
        .collect();
    let retrying = document
        .issues
        .iter()
        .filter_map(|issue| issue.retry.clone())
        .collect();
    Json(StateResponse {
        generated_at: document.generated_at,
        counts: StateResponseCounts {
            running: document.state.counts.running,
            retrying: document.state.counts.retrying,
            sources: document.sources.len(),
        },
        running,
        retrying,
        codex_totals: CodexTotals {
            input_tokens: document.state.codex_totals.input_tokens,
            output_tokens: document.state.codex_totals.output_tokens,
            total_tokens: document.state.codex_totals.total_tokens,
            seconds_running: document.state.seconds_running,
        },
        rate_limits: document.state.rate_limits,
    })
}

async fn sources_api(State(state): State<AppState>) -> Json<SourcesResponse> {
    let document = state.shared_status.snapshot().await;
    Json(SourcesResponse {
        generated_at: document.generated_at,
        sources: document.sources,
    })
}

async fn repositories_api(
    State(state): State<AppState>,
    query: std::result::Result<Query<SourceQuery>, QueryRejection>,
) -> std::result::Result<Json<RepositoryListResponse>, ApiError> {
    let query = query.map_err(|_| ApiError::source_not_found())?.0;
    repository_response_for_source(&state.shared_status, &query.source_id)
        .await
        .map(Json)
}

async fn refresh_api(State(state): State<AppState>) -> (StatusCode, Json<RefreshResponse>) {
    let coalesced = state.refresh_pending.swap(true, Ordering::AcqRel);
    if !coalesced {
        let _ = state.refresh_tx.send(());
    }
    (
        StatusCode::ACCEPTED,
        Json(RefreshResponse {
            queued: true,
            coalesced,
            requested_at: now_utc(),
            operations: ["poll", "reconcile"],
        }),
    )
}

async fn issue_detail_api(
    State(state): State<AppState>,
    AxumPath(issue_identifier): AxumPath<String>,
) -> std::result::Result<Json<IssueDetail>, ApiError> {
    let document = state.shared_status.snapshot().await;
    document
        .issues
        .into_iter()
        .find(|issue| issue.issue_identifier == issue_identifier)
        .map(Json)
        .ok_or_else(ApiError::issue_not_found)
}

async fn repository_response_for_source(
    shared_status: &SharedStatus,
    source_id: &str,
) -> std::result::Result<RepositoryListResponse, ApiError> {
    let workflow_path = workflow_path_for_source(shared_status, source_id).await?;
    let workflow =
        load_workflow(&workflow_path).map_err(|err| workflow_read_failed(err.to_string()))?;
    let config = EffectiveConfig::from_workflow(workflow)
        .map_err(|err| workflow_read_failed(err.to_string()))?;
    let repositories = config
        .tracker
        .github
        .map(|github| github.repositories)
        .unwrap_or_default();
    Ok(RepositoryListResponse {
        source_id: source_id.to_string(),
        workflow_path: workflow_path.display().to_string(),
        repositories,
    })
}

async fn workflow_path_for_source(
    shared_status: &SharedStatus,
    source_id: &str,
) -> std::result::Result<PathBuf, ApiError> {
    let document = shared_status.snapshot().await;
    document
        .sources
        .iter()
        .find(|source| source.source_id == source_id)
        .map(|source| PathBuf::from(&source.workflow_path))
        .ok_or_else(ApiError::source_not_found)
}

fn source_summaries(configs: &[EffectiveConfig]) -> Vec<SourceSummary> {
    configs
        .iter()
        .map(|config| {
            let github = config.tracker.github.as_ref();
            SourceSummary {
                source_id: config.source.id.clone(),
                workflow_path: config.workflow_path.display().to_string(),
                repositories: github
                    .map(|github| github.repositories.clone())
                    .unwrap_or_default(),
                project_owner_type: github.map(|github| github.project_owner_type),
                project_owner_login: github.map(|github| github.project_owner_login.clone()),
                project_number: github.map(|github| github.project_number),
                status_field_name: github.map(|github| github.status_field_name.clone()),
                active_states: config.tracker.active_states.clone(),
                terminal_states: config.tracker.terminal_states.clone(),
                polling_interval_ms: config.polling.interval_ms,
                workspace_root: config.workspace.root.display().to_string(),
            }
        })
        .collect()
}

fn issue_details(snapshot: &RuntimeSnapshot, configs: &[EffectiveConfig]) -> Vec<IssueDetail> {
    let mut issues = Vec::with_capacity(snapshot.counts.running + snapshot.counts.retrying);
    for running in &snapshot.running {
        issues.push(running_issue_detail(running, configs));
    }
    for retry in &snapshot.retrying {
        issues.push(retrying_issue_detail(retry, configs));
    }
    issues
}

fn running_issue_detail(running: &RunningSnapshot, configs: &[EffectiveConfig]) -> IssueDetail {
    let recent_events = running
        .last_event
        .as_ref()
        .zip(running.last_event_at)
        .map(|(event, at)| RecentEvent {
            at,
            event: event.clone(),
            message: running.last_message.clone(),
        })
        .into_iter()
        .collect();
    IssueDetail {
        source_id: running.source_id.clone(),
        issue_identifier: running.issue_identifier.clone(),
        issue_id: running.issue_id.clone(),
        status: if running.cancel_requested {
            "cancel_requested".to_string()
        } else {
            "running".to_string()
        },
        workspace: WorkspaceDetail {
            path: workspace_path(configs, &running.source_id, &running.workspace_key),
            key: running.workspace_key.clone(),
        },
        attempts: AttemptDetail {
            restart_count: running.retry_attempt.unwrap_or(0),
            current_retry_attempt: running.retry_attempt,
        },
        running: Some(RunningDetail {
            session_id: running.session_id.clone(),
            thread_id: running.thread_id.clone(),
            turn_id: running.turn_id.clone(),
            codex_app_server_pid: running.codex_app_server_pid,
            turn_count: running.turn_count,
            state: running.state.clone(),
            started_at: running.started_at,
            last_event: running.last_event.clone().unwrap_or_default(),
            last_message: running.last_message.clone().unwrap_or_default(),
            last_event_at: running.last_event_at,
            tokens: running.tokens.clone(),
        }),
        retry: None,
        logs: LogsDetail::default(),
        recent_events,
        last_error: None,
        tracked: serde_json::Value::Object(Default::default()),
    }
}

fn retrying_issue_detail(retry: &RetrySnapshot, configs: &[EffectiveConfig]) -> IssueDetail {
    IssueDetail {
        source_id: retry.source_id.clone(),
        issue_identifier: retry.issue_identifier.clone(),
        issue_id: retry.issue_id.clone(),
        status: "retrying".to_string(),
        workspace: WorkspaceDetail {
            path: workspace_path(configs, &retry.source_id, &retry.workspace_key),
            key: retry.workspace_key.clone(),
        },
        attempts: AttemptDetail {
            restart_count: retry.attempt,
            current_retry_attempt: Some(retry.attempt),
        },
        running: None,
        retry: Some(retry_detail(retry)),
        logs: LogsDetail::default(),
        recent_events: Vec::new(),
        last_error: retry.error.clone(),
        tracked: serde_json::Value::Object(Default::default()),
    }
}

fn retry_detail(retry: &RetrySnapshot) -> RetryDetail {
    RetryDetail {
        source_id: retry.source_id.clone(),
        issue_id: retry.issue_id.clone(),
        issue_identifier: retry.issue_identifier.clone(),
        workspace_key: retry.workspace_key.clone(),
        attempt: retry.attempt,
        remaining_delay_ms: retry.remaining_delay_ms,
        error: retry.error.clone().unwrap_or_default(),
    }
}

fn workspace_path(configs: &[EffectiveConfig], source_id: &str, workspace_key: &str) -> String {
    configs
        .iter()
        .find(|config| config.source.id == source_id)
        .map(|config| {
            config
                .workspace
                .root
                .join(workspace_key)
                .display()
                .to_string()
        })
        .unwrap_or_else(|| workspace_key.to_string())
}

fn workflow_read_failed(message: String) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "workflow_read_failed",
        message,
    )
}

async fn api_method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method is not supported for this route",
    )
}

async fn api_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "API route is not defined",
    )
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn source_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "source_not_found",
            "source is not loaded",
        )
    }

    fn issue_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "issue_not_found",
            "issue is not known",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Symphony</title>
<style>
:root{color-scheme:light dark;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#111827;color:#f9fafb}body{margin:0;padding:24px}main{max-width:1180px;margin:0 auto}h1{margin:0 0 16px}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin:16px 0}.card,section{background:#1f2937;border:1px solid #374151;border-radius:10px;padding:14px}.card b{display:block;font-size:28px}table{width:100%;border-collapse:collapse;margin-top:10px}th,td{border-bottom:1px solid #374151;padding:8px;text-align:left;vertical-align:top}button{border-radius:7px;border:1px solid #2563eb;background:#2563eb;color:#f9fafb;padding:8px;cursor:pointer}.row{display:flex;gap:12px;align-items:center;flex-wrap:wrap}.banner{min-height:1.3em;margin:8px 0}.banner.error{color:#fca5a5}.banner.ok{color:#86efac}code{color:#bfdbfe}
</style>
</head>
<body>
<main>
<h1>Symphony</h1>
<div id="banner" class="banner"></div>
<div class="row"><button id="refresh">Refresh now</button><span id="generated"></span></div>
<div class="cards">
<div class="card">Sources<b id="sources-count">0</b></div>
<div class="card">Running<b id="running-count">0</b></div>
<div class="card">Retrying<b id="retrying-count">0</b></div>
<div class="card">Total tokens<b id="tokens-count">0</b></div>
<div class="card">Seconds running<b id="seconds-running">0</b></div>
</div>
<section><h2>Sources</h2><table><thead><tr><th>Source</th><th>Repositories</th><th>Workflow</th><th>Project</th><th>States</th><th>Workspace</th></tr></thead><tbody id="sources-body"></tbody></table></section>
<section><h2>Running and retrying</h2><table><thead><tr><th>Source</th><th>Issue</th><th>Status</th><th>Session</th><th>Error</th></tr></thead><tbody id="issues-body"></tbody></table></section>
<p><code>/api/v1/state</code></p>
</main>
<script>
const $=id=>document.getElementById(id);
function banner(msg,kind='ok'){const el=$('banner');el.textContent=msg;el.className='banner '+kind;}
async function api(path,options){const res=await fetch(path,options);const text=await res.text();let data=null;if(text)data=JSON.parse(text);if(!res.ok)throw new Error(data?.error?.message||data?.error?.code||res.statusText);return data;}
function esc(v){return String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function renderSources(sources){$('sources-body').innerHTML=sources.map(s=>`<tr><td>${esc(s.source_id)}</td><td>${esc(s.repositories.map(r=>r.owner+'/'+r.name).join(', '))}</td><td>${esc(s.workflow_path)}</td><td>${esc(s.project_owner_login)} #${esc(s.project_number)}</td><td>Active: ${esc(s.active_states.join(', '))}<br>Terminal: ${esc(s.terminal_states.join(', '))}</td><td>${esc(s.workspace_root)}</td></tr>`).join('');}
function renderIssues(state){const running=state.running.map(i=>({source_id:i.source_id,identifier:i.issue_identifier,status:i.state,session:i.session_id||'',error:''}));const retrying=state.retrying.map(i=>({source_id:i.source_id,identifier:i.issue_identifier,status:'retrying',session:'',error:i.error||''}));$('issues-body').innerHTML=running.concat(retrying).map(i=>`<tr><td>${esc(i.source_id)}</td><td><a href="/api/v1/${encodeURIComponent(i.identifier)}">${esc(i.identifier)}</a></td><td>${esc(i.status)}</td><td>${esc(i.session)}</td><td>${esc(i.error)}</td></tr>`).join('');}
async function loadAll(){try{const [state,sourceDoc]=await Promise.all([api('/api/v1/state'),api('/api/v1/sources')]);$('generated').textContent='Generated '+state.generated_at;$('sources-count').textContent=state.counts.sources;$('running-count').textContent=state.counts.running;$('retrying-count').textContent=state.counts.retrying;$('tokens-count').textContent=state.codex_totals.total_tokens;$('seconds-running').textContent=Math.round(state.codex_totals.seconds_running);renderSources(sourceDoc.sources);renderIssues(state);banner('Loaded','ok');}catch(err){banner(err.message,'error');}}
$('refresh').onclick=async()=>{try{const res=await api('/api/v1/refresh',{method:'POST'});banner('Refresh queued; coalesced='+res.coalesced,'ok');}catch(err){banner(err.message,'error');}};
loadAll();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{SharedStatus, spawn_http_server};
    use crate::domain::{CodexEvent, Issue, TokenTotals};
    use crate::orchestrator::OrchestratorState;
    use crate::time::now_utc;

    fn issue(id: &str, identifier: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: "HTTP status test".to_string(),
            description: None,
            priority: None,
            state: "In Progress".to_string(),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[tokio::test]
    async fn loopback_ephemeral_listener_serves_published_state() {
        let shared_status = SharedStatus::new(&[]);
        let mut state = OrchestratorState::default();
        let running = issue("running-id", "RUN-1");
        let retrying = issue("retry-id", "RETRY-1");
        state.claim_running(running.clone(), None, now_utc());
        state.apply_codex_event(CodexEvent {
            issue_id: running.id.clone(),
            event: "turn_started".to_string(),
            timestamp: now_utc(),
            session_id: Some("session-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            codex_app_server_pid: None,
            message: Some("working".to_string()),
            absolute_token_totals: Some(TokenTotals {
                input_tokens: 12,
                output_tokens: 8,
                total_tokens: 20,
            }),
            rate_limits: Some(json!({"remaining": 42})),
        });
        state.schedule_retry_now(&retrying, 2, Some("transient failure".to_string()));
        shared_status.publish(&state, &[]).await;

        let (refresh_tx, _refresh_rx) = mpsc::unbounded_channel();
        let server = spawn_http_server(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            shared_status,
            refresh_tx,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert!(server.local_addr.ip().is_loopback());
        assert_ne!(server.local_addr.port(), 0);

        let response = reqwest::get(format!("http://{}/api/v1/state", server.local_addr))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(body["generated_at"].is_string());
        assert_eq!(body["counts"]["running"], 1);
        assert_eq!(body["counts"]["retrying"], 1);
        assert_eq!(body["running"][0]["issue_identifier"], "RUN-1");
        assert_eq!(body["retrying"][0]["issue_identifier"], "RETRY-1");
        assert_eq!(body["codex_totals"]["total_tokens"], 20);
        assert_eq!(body["rate_limits"]["remaining"], 42);

        server.task.abort();
        let _ = server.task.await;
    }

    #[tokio::test]
    async fn refresh_coalesces_and_api_errors_are_json() {
        let shared_status = SharedStatus::new(&[]);
        let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel();
        let server = spawn_http_server(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            shared_status,
            refresh_tx,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let endpoint = format!("http://{}", server.local_addr);

        let first: serde_json::Value = client
            .post(format!("{endpoint}/api/v1/refresh"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let second: serde_json::Value = client
            .post(format!("{endpoint}/api/v1/refresh"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(first["queued"], true);
        assert_eq!(first["coalesced"], false);
        assert_eq!(second["coalesced"], true);
        assert!(refresh_rx.try_recv().is_ok());
        assert!(refresh_rx.try_recv().is_err());

        let method_error = client
            .post(format!("{endpoint}/api/v1/state"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            method_error.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        );
        let method_body: serde_json::Value = method_error.json().await.unwrap();
        assert_eq!(method_body["error"]["code"], "method_not_allowed");

        let unknown_error = client
            .get(format!("{endpoint}/api/v1/not/a/route"))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown_error.status(), reqwest::StatusCode::NOT_FOUND);
        let unknown_body: serde_json::Value = unknown_error.json().await.unwrap();
        assert_eq!(unknown_body["error"]["code"], "route_not_found");

        server.task.abort();
        let _ = server.task.await;
    }
}
