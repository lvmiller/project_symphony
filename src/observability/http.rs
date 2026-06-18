//! Local HTTP observability and operator dashboard.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::rejection::{JsonRejection, QueryRejection};
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
use crate::domain::{RetryEntry, RuntimeSnapshot, TokenTotals};
use crate::error::{Result, SymphonyError};
use crate::orchestrator::OrchestratorState;
use crate::time::now_utc;
use crate::workflow::load_workflow;
use crate::workflow_store::{self, RepositoryMutation};

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
    pub workspace_key: String,
    pub workspace_path: String,
    pub current_retry_attempt: Option<u32>,
    pub running: Option<RunningDetail>,
    pub retry: Option<RetryEntry>,
    pub last_error: Option<String>,
    pub recent_events: Vec<RecentEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunningDetail {
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub state: String,
    pub started_at: DateTime<Utc>,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenTotals,
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
    pub running: Vec<crate::domain::RunningSnapshot>,
    pub retrying: Vec<RetryEntry>,
    pub codex_totals: TokenTotals,
    pub seconds_running: f64,
    pub rate_limits: Option<serde_json::Value>,
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

#[derive(Clone, Debug, Deserialize)]
struct RepositoryMutationRequest {
    source_id: String,
    owner: String,
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RepositoryDeleteQuery {
    source_id: String,
    owner: String,
    name: String,
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
        let snapshot = state.snapshot(generated_at);
        let sources = source_summaries(configs);
        let issues = issue_details(state, configs);
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
    Router::new()
        .route("/", get(dashboard))
        .route("/api/v1/state", get(state_api))
        .route("/api/v1/sources", get(sources_api))
        .route(
            "/api/v1/repositories",
            get(repositories_api)
                .post(add_repository_api)
                .delete(delete_repository_api),
        )
        .route("/api/v1/refresh", post(refresh_api))
        .route("/api/v1/{*issue_identifier}", get(issue_detail_api))
        .with_state(state)
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn state_api(State(state): State<AppState>) -> Json<StateResponse> {
    let document = state.shared_status.snapshot().await;
    Json(StateResponse {
        generated_at: document.generated_at,
        counts: StateResponseCounts {
            running: document.state.running.len(),
            retrying: document.state.retrying.len(),
            sources: document.sources.len(),
        },
        running: document.state.running,
        retrying: document.state.retrying,
        codex_totals: document.state.codex_totals,
        seconds_running: document.state.seconds_running,
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

async fn add_repository_api(
    State(state): State<AppState>,
    body: std::result::Result<Json<RepositoryMutationRequest>, JsonRejection>,
) -> std::result::Result<Json<RepositoryListResponse>, ApiError> {
    let request = body
        .map_err(|err| ApiError::invalid_repository(err.to_string()))?
        .0;
    if request.source_id.trim().is_empty() {
        return Err(ApiError::source_not_found());
    }
    let workflow_path = workflow_path_for_source(&state.shared_status, &request.source_id).await?;
    let list = workflow_store::add_repository(
        &workflow_path,
        RepositoryMutation {
            owner: request.owner,
            name: request.name,
        },
    )
    .map_err(store_error)?;
    Ok(Json(RepositoryListResponse {
        source_id: request.source_id,
        workflow_path: workflow_path.display().to_string(),
        repositories: list.repositories,
    }))
}

async fn delete_repository_api(
    State(state): State<AppState>,
    query: std::result::Result<Query<RepositoryDeleteQuery>, QueryRejection>,
) -> std::result::Result<Json<RepositoryListResponse>, ApiError> {
    let request = query
        .map_err(|err| ApiError::invalid_repository(err.to_string()))?
        .0;
    let workflow_path = workflow_path_for_source(&state.shared_status, &request.source_id).await?;
    let list = workflow_store::remove_repository(
        &workflow_path,
        RepositoryMutation {
            owner: request.owner,
            name: request.name,
        },
    )
    .map_err(store_error)?;
    Ok(Json(RepositoryListResponse {
        source_id: request.source_id,
        workflow_path: workflow_path.display().to_string(),
        repositories: list.repositories,
    }))
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
        load_workflow(&workflow_path).map_err(|err| workflow_update_failed(err.to_string()))?;
    let config = EffectiveConfig::from_workflow(workflow)
        .map_err(|err| workflow_update_failed(err.to_string()))?;
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

fn issue_details(state: &OrchestratorState, configs: &[EffectiveConfig]) -> Vec<IssueDetail> {
    let mut issues = Vec::with_capacity(state.running.len() + state.retry_attempts.len());
    for entry in state.running.values() {
        let workspace_path = workspace_path(configs, &entry.source_id, &entry.workspace_key);
        let (running, recent_events) = match &entry.live_session {
            Some(session) => {
                let recent_events = session
                    .last_codex_event
                    .as_ref()
                    .map(|event| RecentEvent {
                        at: session.last_codex_timestamp.unwrap_or(entry.started_at),
                        event: event.clone(),
                        message: session.last_codex_message.clone(),
                    })
                    .into_iter()
                    .collect();
                (
                    RunningDetail {
                        session_id: Some(session.session_id.clone()),
                        turn_count: session.turn_count,
                        state: if entry.cancel_requested {
                            "cancel_requested".to_string()
                        } else {
                            "running".to_string()
                        },
                        started_at: entry.started_at,
                        last_event: session.last_codex_event.clone(),
                        last_message: session.last_codex_message.clone(),
                        last_event_at: session.last_codex_timestamp,
                        tokens: session.codex_tokens.clone(),
                    },
                    recent_events,
                )
            }
            None => (
                RunningDetail {
                    session_id: None,
                    turn_count: 0,
                    state: if entry.cancel_requested {
                        "cancel_requested".to_string()
                    } else {
                        "running".to_string()
                    },
                    started_at: entry.started_at,
                    last_event: None,
                    last_message: None,
                    last_event_at: None,
                    tokens: TokenTotals::default(),
                },
                Vec::new(),
            ),
        };
        issues.push(IssueDetail {
            source_id: entry.source_id.clone(),
            issue_identifier: entry.identifier.clone(),
            issue_id: entry.issue.id.clone(),
            status: entry.issue.state.clone(),
            workspace_key: entry.workspace_key.clone(),
            workspace_path,
            current_retry_attempt: entry.retry_attempt,
            running: Some(running),
            retry: None,
            last_error: None,
            recent_events,
        });
    }
    for retry in state.retry_attempts.values() {
        issues.push(IssueDetail {
            source_id: retry.source_id.clone(),
            issue_identifier: retry.identifier.clone(),
            issue_id: retry.issue_id.clone(),
            status: "retrying".to_string(),
            workspace_key: retry.workspace_key.clone(),
            workspace_path: workspace_path(configs, &retry.source_id, &retry.workspace_key),
            current_retry_attempt: Some(retry.attempt),
            running: None,
            retry: Some(retry.clone()),
            last_error: retry.error.clone(),
            recent_events: Vec::new(),
        });
    }
    issues
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

fn store_error(error: SymphonyError) -> ApiError {
    match error {
        SymphonyError::ConfigValidation { code, message } => match code {
            "invalid_repository" => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, message),
            "duplicate_repository" => ApiError::new(StatusCode::CONFLICT, code, message),
            "repository_not_found" => ApiError::new(StatusCode::NOT_FOUND, code, message),
            "last_repository" => ApiError::new(StatusCode::CONFLICT, code, message),
            _ => workflow_update_failed(format!(
                "config_validation_error code={code} message={message}"
            )),
        },
        other => workflow_update_failed(other.to_string()),
    }
}

fn workflow_update_failed(message: String) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "workflow_update_failed",
        message,
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

    fn invalid_repository(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_repository",
            message,
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
:root{color-scheme:light dark;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#111827;color:#f9fafb}body{margin:0;padding:24px}main{max-width:1180px;margin:0 auto}h1{margin:0 0 16px}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin:16px 0}.card,section{background:#1f2937;border:1px solid #374151;border-radius:10px;padding:14px}.card b{display:block;font-size:28px}table{width:100%;border-collapse:collapse;margin-top:10px}th,td{border-bottom:1px solid #374151;padding:8px;text-align:left;vertical-align:top}input,select,button{border-radius:7px;border:1px solid #4b5563;background:#111827;color:#f9fafb;padding:8px}button{cursor:pointer;background:#2563eb;border-color:#2563eb}button.danger{background:#991b1b;border-color:#991b1b}.row{display:flex;gap:8px;flex-wrap:wrap;align-items:end}.banner{display:none;margin:12px 0;padding:10px;border-radius:8px}.banner.ok,.banner.error{display:block}.banner.ok{background:#064e3b}.banner.error{background:#7f1d1d}code{white-space:pre-wrap}</style>
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
<section><h2>Repository management</h2><form id="repo-form" class="row"><label>Source<br><select id="repo-source"></select></label><label>Owner<br><input id="repo-owner" autocomplete="off" required></label><label>Name<br><input id="repo-name" autocomplete="off" required></label><button type="submit">Add</button></form><div id="repo-lists"></div></section>
<section><h2>Sources</h2><table><thead><tr><th>Source</th><th>Workflow</th><th>Project</th><th>States</th><th>Workspace</th></tr></thead><tbody id="sources-body"></tbody></table></section>
<section><h2>Running and retrying</h2><table><thead><tr><th>Source</th><th>Issue</th><th>Status</th><th>Session</th><th>Error</th></tr></thead><tbody id="issues-body"></tbody></table></section>
<p><code>/api/v1/state</code></p>
</main>
<script>
const $=id=>document.getElementById(id);let sources=[];
function banner(msg,kind='ok'){const el=$('banner');el.textContent=msg;el.className='banner '+kind;}
async function api(path,options){const res=await fetch(path,options);const text=await res.text();let data=null;if(text)data=JSON.parse(text);if(!res.ok){throw new Error(data?.error?.message||data?.error?.code||res.statusText);}return data;}
function esc(v){return String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
async function loadAll(){try{const [state,sourceDoc]=await Promise.all([api('/api/v1/state'),api('/api/v1/sources')]);sources=sourceDoc.sources;$('generated').textContent='Generated '+state.generated_at;$('sources-count').textContent=state.counts.sources;$('running-count').textContent=state.counts.running;$('retrying-count').textContent=state.counts.retrying;$('tokens-count').textContent=state.codex_totals.total_tokens;$('seconds-running').textContent=Math.round(state.seconds_running);renderSources();await renderRepositories();renderIssues(state);banner('Loaded','ok');}catch(err){banner(err.message,'error');}}
function renderSources(){const select=$('repo-source');select.innerHTML=sources.map(s=>`<option value="${esc(s.source_id)}">${esc(s.source_id)}</option>`).join('');$('sources-body').innerHTML=sources.map(s=>`<tr><td>${esc(s.source_id)}</td><td>${esc(s.workflow_path)}</td><td>${esc(s.project_owner_login)} #${esc(s.project_number)}</td><td>Active: ${esc(s.active_states.join(', '))}<br>Terminal: ${esc(s.terminal_states.join(', '))}</td><td>${esc(s.workspace_root)}</td></tr>`).join('');}
async function renderRepositories(){const rows=[];for(const source of sources){const doc=await api('/api/v1/repositories?source_id='+encodeURIComponent(source.source_id));rows.push(`<h3>${esc(source.source_id)}</h3><ul>${doc.repositories.map(r=>`<li>${esc(r.owner)}/${esc(r.name)} <button class="danger" data-source="${esc(source.source_id)}" data-owner="${esc(r.owner)}" data-name="${esc(r.name)}">Remove</button></li>`).join('')}</ul>`);}$('repo-lists').innerHTML=rows.join('');document.querySelectorAll('button.danger').forEach(button=>button.onclick=async()=>{try{await api('/api/v1/repositories?source_id='+encodeURIComponent(button.dataset.source)+'&owner='+encodeURIComponent(button.dataset.owner)+'&name='+encodeURIComponent(button.dataset.name),{method:'DELETE'});await loadAll();}catch(err){banner(err.message,'error');}});}
function renderIssues(state){const running=state.running.map(i=>({source_id:i.source_id,identifier:i.issue_identifier,status:i.state,session:i.session_id||'',error:''}));const retrying=state.retrying.map(i=>({source_id:i.source_id,identifier:i.identifier,status:'retrying',session:'',error:i.error||''}));$('issues-body').innerHTML=running.concat(retrying).map(i=>`<tr><td>${esc(i.source_id)}</td><td><a href="/api/v1/${encodeURIComponent(i.identifier)}">${esc(i.identifier)}</a></td><td>${esc(i.status)}</td><td>${esc(i.session)}</td><td>${esc(i.error)}</td></tr>`).join('');}
$('repo-form').addEventListener('submit',async event=>{event.preventDefault();try{await api('/api/v1/repositories',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({source_id:$('repo-source').value,owner:$('repo-owner').value,name:$('repo-name').value})});$('repo-owner').value='';$('repo-name').value='';await loadAll();}catch(err){banner(err.message,'error');}});
$('refresh').onclick=async()=>{try{const res=await api('/api/v1/refresh',{method:'POST'});banner('Refresh queued; coalesced='+res.coalesced,'ok');}catch(err){banner(err.message,'error');}};
loadAll();
</script>
</body>
</html>
"#;
