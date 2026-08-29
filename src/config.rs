//! Typed workflow configuration.
//!
//! Implementation-defined choices documented here:
//! - Tracker adapter: this Rust implementation supports `tracker.kind: github` and maps GitHub
//!   Projects v2 Status values onto Symphony issue states. Repository-only issues are not dispatched.
//! - Approval/sandbox posture: Codex `approvalPolicy = "never"` and the default sandbox confines
//!   issue-driven work to its active workspace without network or temporary-directory access.
//!   Full access requires the explicit trusted-local `codex.trusted_danger_full_access` opt-in.
//! - Workspace population: Symphony creates/reuses per-issue directories and removes them after a
//!   successful direct-commit completion unless `workspace.cleanup.after_success: never` is set.
//!   Checkout/sync/bootstrap is owned by configured hooks.
//! - Logging sink: structured logs are emitted to stderr.
//! - Existing non-directory workspace path policy: fail safely; never replace user data.
//! - User-input-required policy: the Codex client fails the run rather than waiting indefinitely.
//! - Container runtime: the published image uses an init/reaper, executes hooks and Codex inside the
//!   container namespace, and expects workflow/workspace paths to be container paths.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::domain::WorkflowDefinition;
use crate::error::{Result, SymphonyError};
use crate::workflow::{load_workflow, select_workflow_path};
use crate::workspace::source_workspace_namespace;

pub const DEFAULT_GITHUB_ENDPOINT: &str = "https://api.github.com/graphql";
pub const DEFAULT_PROMPT: &str = "You are working on an issue from GitHub.";

/// Resolves and validates a credential-bearing GitHub endpoint without exposing its text in
/// errors. Plain HTTP is deliberately limited to an explicit numeric-loopback development mode.
pub fn resolve_github_endpoint(
    endpoint: &str,
    allow_insecure_loopback: bool,
) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| {
        SymphonyError::config(
            "invalid_tracker_endpoint",
            "tracker.endpoint must be an absolute HTTP(S) URL without credentials",
        )
    })?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SymphonyError::config(
            "invalid_tracker_endpoint",
            "tracker.endpoint must be an absolute HTTP(S) URL without credentials",
        ));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if allow_insecure_loopback && is_numeric_loopback_host(url.host_str()) => Ok(url),
        "http" => Err(SymphonyError::config(
            "insecure_tracker_endpoint",
            "tracker.endpoint must use HTTPS unless tracker.allow_insecure_loopback is true and the host is numeric IP loopback",
        )),
        _ => Err(SymphonyError::config(
            "invalid_tracker_endpoint",
            "tracker.endpoint must use HTTP or HTTPS",
        )),
    }
}

fn is_numeric_loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = match host.strip_prefix('[') {
        Some(host) => host.strip_suffix(']').unwrap_or_default(),
        None => host,
    };
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}
pub const DEFAULT_SOURCE_ID: &str = "default";

/// A secret-safe result for one configuration diagnostic check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigDiagnosticCheck {
    pub status: ConfigDiagnosticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

impl ConfigDiagnosticCheck {
    fn passed() -> Self {
        Self {
            status: ConfigDiagnosticStatus::Passed,
            error_class: None,
        }
    }

    fn failed(error: &SymphonyError) -> Self {
        Self {
            status: ConfigDiagnosticStatus::Failed,
            error_class: Some(config_reload_error_class(error).to_string()),
        }
    }

    fn not_run() -> Self {
        Self {
            status: ConfigDiagnosticStatus::NotRun,
            error_class: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == ConfigDiagnosticStatus::Passed
    }
}

/// The outcome of a configuration diagnostic check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDiagnosticStatus {
    Passed,
    Failed,
    NotRun,
}

/// Whether an explicitly configured environment value can be used, without retaining its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentValuePresence {
    Present,
    Missing,
    Empty,
}

/// The configured source of a tracker API key, excluding the key itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TrackerApiKeySource {
    Literal,
    Environment { variable: String },
    Missing,
}

/// A secret-safe tracker API key diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrackerApiKeyDiagnostic {
    pub source: TrackerApiKeySource,
    pub presence: EnvironmentValuePresence,
}

/// The configured source of a workspace root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkspaceRootSource {
    Literal,
    Environment { variable: String },
    Default,
}

/// A workspace root diagnostic that never retains a resolved environment value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceRootDiagnostic {
    pub source: WorkspaceRootSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_presence: Option<EnvironmentValuePresence>,
    pub status: ConfigDiagnosticCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_path: Option<PathBuf>,
}

/// Secret-safe diagnostics for loading and dispatching one workflow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkflowDiagnostics {
    pub workflow_path: PathBuf,
    pub parse: ConfigDiagnosticCheck,
    pub dispatch: ConfigDiagnosticCheck,
    pub tracker_api_key: TrackerApiKeyDiagnostic,
    pub workspace_root: WorkspaceRootDiagnostic,
}

impl WorkflowDiagnostics {
    /// Returns whether parsing, workspace resolution, and dispatch validation all succeeded.
    pub fn is_healthy(&self) -> bool {
        self.parse.is_success()
            && self.dispatch.is_success()
            && self.workspace_root.status.is_success()
    }
}

/// Produces workflow diagnostics without retaining or serializing resolved secret values.
///
/// A selected workflow path is required to produce a report; all workflow load, parse, and
/// dispatch failures are instead represented by the returned diagnostic statuses.
pub fn workflow_diagnostics(explicit_path: Option<PathBuf>) -> Result<WorkflowDiagnostics> {
    let workflow_path = select_workflow_path(explicit_path)?;
    let workflow = match load_workflow(&workflow_path) {
        Ok(workflow) => workflow,
        Err(error) => {
            return Ok(WorkflowDiagnostics {
                workflow_path,
                parse: ConfigDiagnosticCheck::failed(&error),
                dispatch: ConfigDiagnosticCheck::not_run(),
                tracker_api_key: TrackerApiKeyDiagnostic {
                    source: TrackerApiKeySource::Missing,
                    presence: EnvironmentValuePresence::Missing,
                },
                workspace_root: default_workspace_root_diagnostic(),
            });
        }
    };

    let tracker_api_key = tracker_api_key_diagnostic(&workflow.config);
    let workspace_root = workspace_root_diagnostic(&workflow.config, workflow.path.parent());
    let config = match EffectiveConfig::from_workflow(workflow) {
        Ok(config) => config,
        Err(error) => {
            return Ok(WorkflowDiagnostics {
                workflow_path,
                parse: ConfigDiagnosticCheck::failed(&error),
                dispatch: ConfigDiagnosticCheck::not_run(),
                tracker_api_key,
                workspace_root,
            });
        }
    };
    let dispatch = match config.validate_dispatch() {
        Ok(()) => ConfigDiagnosticCheck::passed(),
        Err(error) => ConfigDiagnosticCheck::failed(&error),
    };

    Ok(WorkflowDiagnostics {
        workflow_path,
        parse: ConfigDiagnosticCheck::passed(),
        dispatch,
        tracker_api_key,
        workspace_root,
    })
}

/// Returns a stable, secret-safe classification for a configuration or reload error.
pub fn config_reload_error_class(error: &SymphonyError) -> &'static str {
    match error {
        SymphonyError::MissingWorkflowFile { .. } => "missing_workflow_file",
        SymphonyError::WorkflowPathNotFile { .. } => "workflow_path_not_file",
        SymphonyError::WorkflowParseError { .. } => "workflow_parse_error",
        SymphonyError::WorkflowFrontMatterNotMap { .. } => "workflow_front_matter_not_a_map",
        SymphonyError::ConfigValidation { code, .. } => code,
        SymphonyError::UnsupportedTrackerKind { .. } => "unsupported_tracker_kind",
        SymphonyError::MissingTrackerApiKey => "missing_tracker_api_key",
        SymphonyError::MissingGithubConfig { .. } => "missing_github_config",
        SymphonyError::TemplateParseError(_) => "template_parse_error",
        SymphonyError::TemplateRenderError(_) => "template_render_error",
        SymphonyError::Workspace(_) => "workspace_error",
        SymphonyError::Hook { .. } => "hook_error",
        SymphonyError::Tracker { .. } => "tracker_error",
        SymphonyError::Codex { .. } => "codex_error",
        SymphonyError::Io { .. } => "io_error",
        SymphonyError::Yaml(_) => "yaml_error",
        SymphonyError::Json(_) => "json_error",
        SymphonyError::Http(_) => "http_error",
    }
}

/// Returns the stable JSON Schema for raw `WORKFLOW.md` YAML front matter.
///
/// This describes accepted source keys rather than [`EffectiveConfig`], which contains resolved
/// paths and secrets. The schema is static: it neither loads a workflow nor reads environment
/// variables, so secret values cannot appear in its output.
pub fn raw_workflow_json_schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Symphony workflow front matter",
        "description": "Raw YAML front matter for WORKFLOW.md. Unknown top-level keys are accepted as forward-compatible extensions. Fields documented as supporting `$VAR_NAME` resolve that environment variable at workflow load time; this schema never reads or embeds environment values. Required dispatch fields are intentionally not globally required because schema validation also supports incomplete editor drafts.",
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "source": {
                "type": "object",
                "description": "Optional source identity. Changes apply to future dispatch after dynamic reload.",
                "properties": {
                    "id": { "type": "string", "minLength": 1, "default": DEFAULT_SOURCE_ID }
                },
                "additionalProperties": true
            },
            "tracker": {
                "type": "object",
                "description": "Issue tracker configuration. `kind`, credentials, repository, and project fields are required only for dispatch.",
                "properties": {
                    "kind": { "type": "string", "enum": ["github"], "description": "Required for dispatch." },
                    "endpoint": { "type": "string", "format": "uri", "default": DEFAULT_GITHUB_ENDPOINT, "description": "HTTPS GitHub GraphQL endpoint when kind is github. HTTP is allowed only for numeric IP loopback when tracker.allow_insecure_loopback is explicitly true." },
                    "allow_insecure_loopback": { "type": "boolean", "default": false, "description": "Development-only exception permitting HTTP only for 127.0.0.0/8 or ::1. Hostnames and private or LAN addresses are never allowed over HTTP." },
                    "api_key": { "type": "string", "writeOnly": true, "description": "Literal credential or `$VAR_NAME`; `$GITHUB_TOKEN` is used by default for GitHub. No secret value is included in this schema." },
                    "terminal_states": { "type": "array", "items": { "type": "string" }, "default": ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"] },
                    "repository": {
                        "type": "object",
                        "description": "Required for GitHub dispatch unless `repositories` is used.",
                        "properties": {
                            "owner": { "type": "string", "minLength": 1 },
                            "name": { "type": "string", "minLength": 1 }
                        },
                        "additionalProperties": true
                    },
                    "repositories": {
                        "type": "array",
                        "description": "Alternative to `repository`; cannot be combined with it.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "owner": { "type": "string", "minLength": 1 },
                                "name": { "type": "string", "minLength": 1 }
                            },
                            "additionalProperties": true
                        }
                    },
                    "project": {
                        "type": "object",
                        "description": "Required for GitHub dispatch.",
                        "properties": {
                            "owner_type": { "type": "string", "enum": ["organization", "org", "user"], "default": "organization" },
                            "owner_login": { "type": "string", "minLength": 1 },
                            "number": { "type": "integer", "minimum": 1 },
                            "status_field": { "type": "string", "minLength": 1, "default": "Status" },
                            "priority_field": { "type": "string", "minLength": 1, "default": "Priority" },
                            "blocker_field": { "type": "string", "minLength": 1 },
                            "blocker_label_prefix": { "type": "string", "minLength": 1 }
                        },
                        "additionalProperties": true
                    },
                    "priority_labels": {
                        "type": "object",
                        "additionalProperties": { "type": "integer" },
                        "default": {}
                    }
                },
                "additionalProperties": true
            },
            "polling": {
                "type": "object",
                "description": "Changes apply to future tick scheduling after dynamic reload.",
                "properties": {
                    "interval_ms": { "type": "integer", "minimum": 1, "default": 30000 }
                },
                "additionalProperties": true
            },
            "worker": {
                "type": "object",
                "description": "Optional SSH worker pool. Omit ssh_hosts to keep execution local.",
                "properties": {
                    "ssh_hosts": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "default": []
                    },
                    "max_concurrent_agents_per_host": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 1
                    }
                },
                "additionalProperties": true
            },
            "workspace": {
                "type": "object",
                "description": "Changes apply to future workspace preparation after dynamic reload.",
                "properties": {
                    "root": { "type": "string", "description": "Workspace path. Supports `~` and `$VAR_NAME`; relative paths resolve from WORKFLOW.md. Default is `<system-temp>/symphony_workspaces`." },
                    "cleanup": {
                        "type": "object",
                        "properties": {
                            "after_success": { "type": "string", "enum": ["committed", "never"], "default": "committed" }
                        },
                        "additionalProperties": true
                    },
                    "retention": {
                        "type": "object",
                        "description": "Optional startup-only orphaned workspace pruning. Omit max_age_days to disable scanning.",
                        "properties": {
                            "max_age_days": { "type": "integer", "minimum": 1 }
                        },
                        "additionalProperties": true
                    },
                    "population": {
                        "type": "object",
                        "description": "Built-in workspace population. Git requires a non-empty repository_url; `ref` and `branch` are mutually exclusive.",
                        "properties": {
                            "kind": { "type": "string", "enum": ["none", "git"], "default": "none" },
                            "repository_url": { "type": "string", "minLength": 1, "format": "uri" },
                            "ref": { "type": "string", "minLength": 1 },
                            "branch": { "type": "string", "minLength": 1 },
                            "depth": { "type": "integer", "minimum": 1 },
                            "reuse": { "type": "string", "enum": ["skip", "fetch_ff_only"], "default": "skip" }
                        },
                        "additionalProperties": true
                    }
                },
                "additionalProperties": true
            },
            "hooks": {
                "type": "object",
                "description": "Workspace hook configuration. Changes apply to future hook execution after dynamic reload.",
                "properties": {
                    "after_create": { "type": "string" },
                    "before_run": { "type": "string" },
                    "after_run": { "type": "string" },
                    "before_remove": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "default": 60000 }
                },
                "additionalProperties": true
            },
            "agent": {
                "type": "object",
                "description": "Changes apply to future agent dispatch after dynamic reload.",
                "properties": {
                    "max_concurrent_agents": { "type": "integer", "minimum": 1, "default": 10 },
                    "max_turns": { "type": "integer", "minimum": 1, "maximum": 4_294_967_295u64, "default": 20 },
                    "max_retry_backoff_ms": { "type": "integer", "minimum": 1, "default": 300000 },
                    "max_concurrent_agents_by_state": {
                        "type": "object",
                        "additionalProperties": { "type": "integer", "minimum": 1 },
                        "default": {}
                    }
                },
                "additionalProperties": true
            },
            "codex": {
                "type": "object",
                "description": "Changes apply to future agent launches after dynamic reload.",
                "properties": {
                    "command": { "type": "string", "minLength": 1, "default": "codex app-server" },
                    "approval_policy": { "description": "Pass-through Codex approval policy.", "default": "never" },
                    "trusted_danger_full_access": { "type": "boolean", "default": false, "description": "Required before codex.thread_sandbox: danger-full-access or codex.turn_sandbox_policy.type: dangerFullAccess may be used. Set only in trusted operator-controlled workflow configuration." },
                    "thread_sandbox": { "description": "Pass-through Codex thread sandbox.", "default": "workspace-write" },
                    "turn_sandbox_policy": { "description": "Pass-through Codex turn sandbox policy. When omitted, Symphony materializes this restrictive default with writableRoots set to the active canonical workspace immediately before turn/start.", "default": { "type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true } },
                    "turn_timeout_ms": { "type": "integer", "minimum": 1, "default": 3600000 },
                    "read_timeout_ms": { "type": "integer", "minimum": 1, "default": 5000 },
                    "stall_timeout_ms": { "type": "integer", "default": 300000 }
                },
                "additionalProperties": true
            },
            "completion": {
                "type": "object",
                "properties": {
                    "direct_commit": {
                        "type": "object",
                        "properties": {
                            "enabled": { "type": "boolean", "default": false },
                            "dry_run": { "type": "boolean", "default": false, "description": "Perform completion checks without mutating Git or the tracker." },
                            "base_branch": { "type": "string", "minLength": 1, "default": "main" },
                            "high_review_state": { "type": "string", "minLength": 1, "default": "In review" },
                            "auto_approved_state": { "type": "string", "minLength": 1, "default": "Done" },
                            "started_state": { "type": "string", "minLength": 1 },
                            "commit_author_name": { "type": "string", "minLength": 1, "default": "Symphony" },
                            "commit_author_email": { "type": "string", "minLength": 1, "default": "symphony@users.noreply.github.com" }
                        },
                        "additionalProperties": true
                    }
                },
                "additionalProperties": true
            },
            "server": {
                "type": "object",
                "description": "Changing listener settings may require restart.",
                "properties": {
                    "host": { "type": "string", "default": "127.0.0.1", "description": "IP address." },
                    "port": { "type": "integer", "minimum": 0, "maximum": 65535 },
                    "auth_token": { "type": "string", "writeOnly": true, "description": "Optional literal credential or `$VAR_NAME`. Required when server.host is non-loopback. No secret value is included in this schema." },
                    "refresh_cooldown_ms": { "type": "integer", "minimum": 1, "default": 1000, "description": "Minimum delay between accepted operator mutations." },
                    "drain_timeout_ms": { "type": "integer", "minimum": 1, "default": 30000, "description": "Maximum graceful-drain wait during shutdown before remaining workers are cancelled." }
                },
                "additionalProperties": true
            }
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConfig {
    pub id: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: Option<u16>,
    #[serde(skip_serializing)]
    pub auth_token: Option<String>,
    pub refresh_cooldown_ms: u64,
    pub drain_timeout_ms: u64,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_cooldown_ms", &self.refresh_cooldown_ms)
            .field("drain_timeout_ms", &self.drain_timeout_ms)
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::from(Ipv4Addr::LOCALHOST),
            port: None,
            auth_token: None,
            refresh_cooldown_ms: 1_000,
            drain_timeout_ms: 30_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfig {
    pub workflow_path: PathBuf,
    pub workflow_dir: PathBuf,
    pub prompt_template: String,
    pub source: SourceConfig,
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub worker: WorkerConfig,
    pub hooks: HooksConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub completion: CompletionConfig,
    pub server: ServerConfig,
}

impl EffectiveConfig {
    pub fn from_workflow(workflow: WorkflowDefinition) -> Result<Self> {
        let workflow_dir = workflow
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let source = parse_source(&workflow.config)?;
        let tracker = parse_tracker(&workflow.config)?;
        let polling = parse_polling(&workflow.config)?;
        let worker = parse_worker(&workflow.config)?;
        let workspace = parse_workspace(&workflow.config, &workflow_dir)?;
        if !worker.ssh_hosts.is_empty() && !workspace.remote_root.starts_with('/') {
            return Err(SymphonyError::config(
                "invalid_remote_workspace_root",
                "workspace.root must be an absolute POSIX path when worker.ssh_hosts is configured",
            ));
        }
        let hooks = parse_hooks(&workflow.config)?;
        let agent = parse_agent(&workflow.config)?;
        let codex = parse_codex(&workflow.config)?;
        let completion = parse_completion(&workflow.config)?;
        let server = parse_server(&workflow.config)?;
        Ok(Self {
            workflow_path: workflow.path,
            workflow_dir,
            prompt_template: workflow.prompt_template,
            source,
            tracker,
            polling,
            workspace,
            worker,
            hooks,
            agent,
            codex,
            completion,
            server,
        })
    }

    pub fn load(explicit_path: Option<PathBuf>) -> Result<Self> {
        let path = select_workflow_path(explicit_path)?;
        Self::from_workflow(load_workflow(&path)?)
    }

    pub fn prompt_template_or_default(&self) -> &str {
        if self.prompt_template.trim().is_empty() {
            DEFAULT_PROMPT
        } else {
            &self.prompt_template
        }
    }

    pub fn validate_dispatch(&self) -> Result<()> {
        if self.tracker.kind != "github" {
            if self.tracker.kind.is_empty() {
                return Err(SymphonyError::UnsupportedTrackerKind {
                    kind: "".to_string(),
                });
            }
            return Err(SymphonyError::UnsupportedTrackerKind {
                kind: self.tracker.kind.clone(),
            });
        }
        if self
            .tracker
            .api_key
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        {
            return Err(SymphonyError::MissingTrackerApiKey);
        }
        let github = self
            .tracker
            .github
            .as_ref()
            .ok_or(SymphonyError::MissingGithubConfig { field: "github" })?;
        github.validate()?;
        self.tracker.github_endpoint_url()?;
        if self.codex.command.trim().is_empty() {
            return Err(SymphonyError::config(
                "missing_codex_command",
                "codex.command must be non-empty",
            ));
        }
        Ok(())
    }

    pub fn is_active_state(&self, state: &str) -> bool {
        let normalized = normalize_state(state);
        self.tracker
            .active_states
            .iter()
            .any(|configured| normalize_state(configured) == normalized)
    }

    pub fn is_terminal_state(&self, state: &str) -> bool {
        let normalized = normalize_state(state);
        self.tracker
            .terminal_states
            .iter()
            .any(|configured| normalize_state(configured) == normalized)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
    pub kind: String,
    pub endpoint: String,
    pub allow_insecure_loopback: bool,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub github: Option<GithubConfig>,
}

impl TrackerConfig {
    pub fn github_endpoint_url(&self) -> Result<reqwest::Url> {
        resolve_github_endpoint(&self.endpoint, self.allow_insecure_loopback)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubRepositoryConfig {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubConfig {
    pub repository_owner: String,
    pub repository_name: String,
    pub repositories: Vec<GithubRepositoryConfig>,
    pub project_owner_type: GithubProjectOwnerType,
    pub project_owner_login: String,
    pub project_number: i64,
    pub status_field_name: String,
    pub priority_field_name: Option<String>,
    pub blocker_field_name: Option<String>,
    pub blocker_label_prefix: Option<String>,
    pub priority_labels: BTreeMap<String, i64>,
}

impl GithubConfig {
    pub fn validate(&self) -> Result<()> {
        if self.repositories.is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "repository",
            });
        }
        if self.repository_owner.trim().is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "repository.owner",
            });
        }
        if self.repository_name.trim().is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "repository.name",
            });
        }
        for repository in &self.repositories {
            if repository.owner.trim().is_empty() {
                return Err(SymphonyError::MissingGithubConfig {
                    field: "repository.owner",
                });
            }
            if repository.name.trim().is_empty() {
                return Err(SymphonyError::MissingGithubConfig {
                    field: "repository.name",
                });
            }
        }
        if self.project_owner_login.trim().is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "project.owner_login",
            });
        }
        if self.project_number <= 0 {
            return Err(SymphonyError::MissingGithubConfig {
                field: "project.number",
            });
        }
        if self.status_field_name.trim().is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "project.status_field",
            });
        }
        Ok(())
    }

    pub fn issue_matches_configured_repository(&self, owner: &str, name: &str) -> bool {
        self.repositories.iter().any(|repository| {
            repository.owner.eq_ignore_ascii_case(owner)
                && repository.name.eq_ignore_ascii_case(name)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubProjectOwnerType {
    User,
    Organization,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// SSH destinations used for remote worker execution. An empty list keeps all work local.
    pub ssh_hosts: Vec<String>,
    /// Maximum active agents on each configured SSH host.
    pub max_concurrent_agents_per_host: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            ssh_hosts: Vec::new(),
            max_concurrent_agents_per_host: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    /// POSIX root interpreted on SSH worker hosts. It is ignored for local execution.
    pub remote_root: String,
    pub cleanup: WorkspaceCleanupConfig,
    pub retention: WorkspaceRetentionConfig,
    pub population: WorkspacePopulationConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCleanupConfig {
    pub after_success: WorkspaceCleanupAfterSuccess,
}

impl Default for WorkspaceCleanupConfig {
    fn default() -> Self {
        Self {
            after_success: WorkspaceCleanupAfterSuccess::Committed,
        }
    }
}

impl WorkspaceCleanupConfig {
    pub fn removes_after_committed_success(&self) -> bool {
        matches!(self.after_success, WorkspaceCleanupAfterSuccess::Committed)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRetentionConfig {
    pub max_age_days: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCleanupAfterSuccess {
    Never,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePopulationConfig {
    pub kind: WorkspacePopulationKind,
    pub repository_url: Option<String>,
    pub reference: Option<String>,
    pub branch: Option<String>,
    pub depth: Option<u64>,
    pub reuse: WorkspacePopulationReusePolicy,
}

impl Default for WorkspacePopulationConfig {
    fn default() -> Self {
        Self {
            kind: WorkspacePopulationKind::None,
            repository_url: None,
            reference: None,
            branch: None,
            depth: None,
            reuse: WorkspacePopulationReusePolicy::Skip,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspacePopulationKind {
    #[default]
    None,
    Git,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePopulationReusePolicy {
    #[default]
    Skip,
    FetchFfOnly,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: Option<serde_json::Value>,
    #[serde(default)]
    pub trusted_danger_full_access: bool,
    pub thread_sandbox: Option<serde_json::Value>,
    pub turn_sandbox_policy: Option<serde_json::Value>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

impl CodexConfig {
    /// Returns the configured policy unchanged, or materializes the restrictive per-workspace
    /// default immediately before it is sent to the Codex app-server.
    pub fn turn_sandbox_policy_for_workspace(&self, workspace: String) -> serde_json::Value {
        self.turn_sandbox_policy.clone().unwrap_or_else(|| {
            serde_json::json!({
                "type": "workspaceWrite",
                "writableRoots": [workspace],
                "networkAccess": false,
                "excludeTmpdirEnvVar": true,
                "excludeSlashTmp": true,
            })
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionConfig {
    pub direct_commit: DirectCommitCompletionConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCommitCompletionConfig {
    pub enabled: bool,
    pub dry_run: bool,
    pub base_branch: String,
    pub high_review_state: String,
    pub auto_approved_state: String,
    pub started_state: Option<String>,
    pub commit_author_name: String,
    pub commit_author_email: String,
}

impl Default for DirectCommitCompletionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: false,
            base_branch: "main".to_string(),
            high_review_state: "In review".to_string(),
            auto_approved_state: "Done".to_string(),
            started_state: None,
            commit_author_name: "Symphony".to_string(),
            commit_author_email: "symphony@users.noreply.github.com".to_string(),
        }
    }
}

pub struct ConfigReloader {
    path: PathBuf,
    last_good: EffectiveConfig,
    last_modified: Option<SystemTime>,
    _watcher: Option<RecommendedWatcher>,
}

impl ConfigReloader {
    pub fn new(path: Option<PathBuf>) -> Result<Self> {
        let selected = select_workflow_path(path)?;
        let last_good = EffectiveConfig::from_workflow(load_workflow(&selected)?)?;
        last_good.validate_dispatch()?;
        let last_modified = modified_time(&selected).ok();
        Ok(Self {
            path: selected,
            last_good,
            last_modified,
            _watcher: None,
        })
    }

    pub fn current(&self) -> &EffectiveConfig {
        &self.last_good
    }

    pub fn source_id(&self) -> &str {
        &self.last_good.source.id
    }

    /// Returns the configured workflow path, including when a failed reload keeps the last good config.
    pub fn workflow_path(&self) -> &Path {
        &self.path
    }

    pub fn reload_now(&mut self) -> Result<&EffectiveConfig> {
        let next = EffectiveConfig::from_workflow(load_workflow(&self.path)?)?;
        next.validate_dispatch()?;
        if next.source.id != self.last_good.source.id {
            return Err(SymphonyError::config(
                "source_id_change_requires_restart",
                "source.id cannot change during dynamic reload",
            ));
        }
        self.last_modified = modified_time(&self.path).ok();
        self.last_good = next;
        Ok(&self.last_good)
    }

    pub fn reload_if_changed(&mut self) -> Result<bool> {
        let modified = modified_time(&self.path).ok();
        if modified.is_some() && modified == self.last_modified {
            return Ok(false);
        }
        self.reload_now()?;
        Ok(true)
    }

    pub fn start_notify_watcher(&mut self, on_change: impl Fn() + Send + 'static) -> Result<()> {
        let path = self.path.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    on_change();
                }
            })
            .map_err(|err| SymphonyError::config("workflow_watch_error", err.to_string()))?;
        watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(|err| SymphonyError::config("workflow_watch_error", err.to_string()))?;
        self._watcher = Some(watcher);
        Ok(())
    }
}

pub struct ConfigSetReloader {
    reloaders: Vec<ConfigReloader>,
}

impl ConfigSetReloader {
    pub fn new(paths: Vec<PathBuf>) -> Result<Self> {
        let paths = if paths.is_empty() {
            vec![PathBuf::from("WORKFLOW.md")]
        } else {
            paths
        };
        let mut reloaders = Vec::with_capacity(paths.len());
        for path in paths {
            reloaders.push(ConfigReloader::new(Some(path))?);
        }
        validate_unique_source_ids(&reloaders)?;
        Ok(Self { reloaders })
    }

    pub fn from_single(reloader: ConfigReloader) -> Self {
        Self {
            reloaders: vec![reloader],
        }
    }

    pub fn current(&self) -> impl Iterator<Item = &EffectiveConfig> {
        self.reloaders.iter().map(ConfigReloader::current)
    }

    pub fn current_cloned(&self) -> Vec<EffectiveConfig> {
        self.current().cloned().collect()
    }

    pub fn poll_interval_ms(&self) -> u64 {
        self.current()
            .map(|config| config.polling.interval_ms)
            .min()
            .unwrap_or(30_000)
    }

    pub fn initial_server_bind(
        &self,
        cli_host: Option<IpAddr>,
        cli_port: Option<u16>,
    ) -> Result<Option<SocketAddr>> {
        let enabled: Vec<&EffectiveConfig> = self
            .current()
            .filter(|config| config.server.port.is_some())
            .collect();
        if cli_port.is_none() && enabled.is_empty() {
            return Ok(None);
        }

        let port = match cli_port {
            Some(port) => port,
            None => {
                let first = enabled[0].server.port.expect("enabled configs have ports");
                if enabled
                    .iter()
                    .any(|config| config.server.port != Some(first))
                {
                    return Err(conflicting_server_bind());
                }
                first
            }
        };

        let host = match cli_host {
            Some(host) => host,
            None if enabled.is_empty() => ServerConfig::default().host,
            None => {
                let first = enabled[0].server.host;
                if enabled.iter().any(|config| config.server.host != first) {
                    return Err(conflicting_server_bind());
                }
                first
            }
        };

        Ok(Some(SocketAddr::new(host, port)))
    }

    pub fn reload_if_changed(&mut self) -> Vec<(String, PathBuf, Result<bool>)> {
        self.reloaders
            .iter_mut()
            .map(|reloader| {
                let source_id = reloader.source_id().to_string();
                let workflow_path = reloader.workflow_path().to_path_buf();
                (source_id, workflow_path, reloader.reload_if_changed())
            })
            .collect()
    }
}

fn conflicting_server_bind() -> SymphonyError {
    SymphonyError::config(
        "conflicting_server_bind",
        "multiple workflow sources configure different server host/port values; use CLI --host/--port to override",
    )
}

fn validate_unique_source_ids(reloaders: &[ConfigReloader]) -> Result<()> {
    let mut source_ids = BTreeMap::new();
    let mut source_segments = BTreeMap::new();
    for reloader in reloaders {
        let source_id = reloader.source_id();
        let workflow_path = &reloader.current().workflow_path;
        if let Some(previous_path) = source_ids.insert(source_id.to_string(), workflow_path.clone())
        {
            return Err(SymphonyError::config(
                "duplicate_source_id",
                format!(
                    "source.id {source_id:?} is configured by both {} and {}",
                    previous_path.display(),
                    workflow_path.display()
                ),
            ));
        }
        let source_segment = source_workspace_namespace(source_id);
        if let Some(previous_path) =
            source_segments.insert(source_segment.clone(), workflow_path.clone())
        {
            return Err(SymphonyError::config(
                "duplicate_source_workspace_namespace",
                format!(
                    "source.id {source_id:?} shares workspace namespace {source_segment:?} with {}",
                    previous_path.display()
                ),
            ));
        }
    }
    Ok(())
}
fn modified_time(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

fn parse_source(config: &Mapping) -> Result<SourceConfig> {
    let source = get_map(config, "source");
    let raw_id = get_string(source, "id")
        .or_else(|| get_string(Some(config), "source_id"))
        .unwrap_or_else(|| DEFAULT_SOURCE_ID.to_string());
    let id = raw_id.trim();
    if id.is_empty() {
        return Err(SymphonyError::config(
            "invalid_source_id",
            "source.id must be non-empty",
        ));
    }
    Ok(SourceConfig { id: id.to_string() })
}

fn parse_tracker(config: &Mapping) -> Result<TrackerConfig> {
    let tracker = get_map(config, "tracker");
    let kind = get_string(tracker, "kind")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let raw_endpoint = get_string(tracker, "endpoint").unwrap_or_else(|| {
        if kind == "github" {
            DEFAULT_GITHUB_ENDPOINT.to_string()
        } else {
            String::new()
        }
    });
    let allow_insecure_loopback = get_bool(tracker, "allow_insecure_loopback").unwrap_or(false);
    let endpoint = if kind == "github" {
        resolve_github_endpoint(&raw_endpoint, allow_insecure_loopback)?.to_string()
    } else {
        raw_endpoint
    };
    let raw_api_key = get_string(tracker, "api_key")
        .or_else(|| get_string(tracker, "token"))
        .or_else(|| (kind == "github").then(|| "$GITHUB_TOKEN".to_string()));
    let api_key = raw_api_key.and_then(|value| resolve_secret(&value));
    let active_states = get_string_list(tracker, "active_states")
        .unwrap_or_else(|| vec!["Todo".to_string(), "In Progress".to_string()]);
    let terminal_states = get_string_list(tracker, "terminal_states").unwrap_or_else(|| {
        vec![
            "Closed".to_string(),
            "Cancelled".to_string(),
            "Canceled".to_string(),
            "Duplicate".to_string(),
            "Done".to_string(),
        ]
    });
    let github = if kind == "github" {
        Some(parse_github_config(tracker)?)
    } else {
        None
    };
    Ok(TrackerConfig {
        kind,
        endpoint,
        allow_insecure_loopback,
        api_key,
        active_states,
        terminal_states,
        github,
    })
}

fn parse_github_config(tracker: Option<&Mapping>) -> Result<GithubConfig> {
    let repository = get_nested_map(tracker, "repository");
    let project = get_nested_map(tracker, "project");
    let repositories = parse_github_repositories(tracker, repository)?;
    let primary_repository = repositories
        .first()
        .ok_or(SymphonyError::MissingGithubConfig {
            field: "repository",
        })?;
    let repository_owner = primary_repository.owner.clone();
    let repository_name = primary_repository.name.clone();
    let project_owner_login = get_string(project, "owner_login")
        .or_else(|| get_string(project, "owner"))
        .or_else(|| get_string(tracker, "project_owner_login"))
        .unwrap_or_default();
    let owner_type_string = get_string(project, "owner_type")
        .or_else(|| get_string(tracker, "project_owner_type"))
        .unwrap_or_else(|| "organization".to_string());
    let project_owner_type = match owner_type_string.to_ascii_lowercase().as_str() {
        "user" => GithubProjectOwnerType::User,
        "organization" | "org" => GithubProjectOwnerType::Organization,
        _ => {
            return Err(SymphonyError::config(
                "invalid_github_project_owner_type",
                "project owner_type must be user or organization",
            ));
        }
    };
    let project_number = get_i64(project, "number")
        .or_else(|| get_i64(tracker, "project_number"))
        .unwrap_or(0);
    let status_field_name = get_string(project, "status_field")
        .or_else(|| get_string(project, "status_field_name"))
        .or_else(|| get_string(tracker, "status_field_name"))
        .unwrap_or_else(|| "Status".to_string());
    let priority_field_name = get_string(project, "priority_field")
        .or_else(|| get_string(project, "priority_field_name"))
        .or_else(|| get_string(tracker, "priority_field_name"))
        .or_else(|| Some("Priority".to_string()));
    let blocker_field_name = get_string(project, "blocker_field")
        .or_else(|| get_string(project, "blocker_field_name"))
        .or_else(|| get_string(tracker, "blocker_field_name"));
    let blocker_label_prefix = get_string(project, "blocker_label_prefix")
        .or_else(|| get_string(tracker, "blocker_label_prefix"));
    let priority_labels = get_i64_map(tracker, "priority_labels");
    let result = GithubConfig {
        repository_owner,
        repository_name,
        repositories,
        project_owner_type,
        project_owner_login,
        project_number,
        status_field_name,
        priority_field_name,
        blocker_field_name,
        blocker_label_prefix,
        priority_labels,
    };
    result.validate()?;
    Ok(result)
}

fn parse_github_repositories(
    tracker: Option<&Mapping>,
    repository: Option<&Mapping>,
) -> Result<Vec<GithubRepositoryConfig>> {
    let has_single_repository = repository.is_some()
        || get_string(tracker, "repository_owner").is_some()
        || get_string(tracker, "repository_name").is_some();
    if let Some(value) = get_value(tracker, "repositories") {
        if has_single_repository {
            return Err(SymphonyError::config(
                "invalid_github_repository_config",
                "tracker.repository and tracker.repositories cannot both be set",
            ));
        }
        let Some(items) = value.as_sequence() else {
            return Err(SymphonyError::config(
                "invalid_github_repositories",
                "tracker.repositories must be a list",
            ));
        };
        if items.is_empty() {
            return Err(SymphonyError::MissingGithubConfig {
                field: "repositories",
            });
        }
        let mut repositories = Vec::with_capacity(items.len());
        for item in items {
            let Some(mapping) = item.as_mapping() else {
                return Err(SymphonyError::config(
                    "invalid_github_repositories",
                    "tracker.repositories entries must be maps",
                ));
            };
            repositories.push(GithubRepositoryConfig {
                owner: get_string(Some(mapping), "owner").unwrap_or_default(),
                name: get_string(Some(mapping), "name").unwrap_or_default(),
            });
        }
        return Ok(repositories);
    }

    Ok(vec![GithubRepositoryConfig {
        owner: get_string(repository, "owner")
            .or_else(|| get_string(tracker, "repository_owner"))
            .unwrap_or_default(),
        name: get_string(repository, "name")
            .or_else(|| get_string(tracker, "repository_name"))
            .unwrap_or_default(),
    }])
}

fn parse_polling(config: &Mapping) -> Result<PollingConfig> {
    let polling = get_map(config, "polling");
    let interval_ms = get_i64(polling, "interval_ms").unwrap_or(30_000);
    if interval_ms <= 0 {
        return Err(SymphonyError::config(
            "invalid_polling_interval_ms",
            "polling.interval_ms must be positive",
        ));
    }
    Ok(PollingConfig {
        interval_ms: interval_ms as u64,
    })
}

fn parse_worker(config: &Mapping) -> Result<WorkerConfig> {
    let worker = get_map(config, "worker");
    let hosts = match get_value(worker, "ssh_hosts") {
        None => Vec::new(),
        Some(Value::Sequence(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|host| !host.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        SymphonyError::config(
                            "invalid_worker_ssh_hosts",
                            "worker.ssh_hosts must contain non-empty strings",
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(SymphonyError::config(
                "invalid_worker_ssh_hosts",
                "worker.ssh_hosts must be a list of SSH host strings",
            ));
        }
    };
    let mut seen_hosts = BTreeMap::new();
    for host in &hosts {
        if !host.is_ascii()
            || host.starts_with('-')
            || host
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte == b'\0')
        {
            return Err(SymphonyError::config(
                "invalid_worker_ssh_host",
                "worker.ssh_hosts entries must be ASCII, non-option, whitespace-free SSH host strings",
            ));
        }
        let collision_key = host.to_ascii_lowercase();
        if seen_hosts.insert(collision_key, host).is_some() {
            return Err(SymphonyError::config(
                "duplicate_worker_ssh_host",
                "worker.ssh_hosts entries must be unique without case distinctions",
            ));
        }
    }
    let max_concurrent_agents_per_host = positive_u64_or_default(
        worker,
        "max_concurrent_agents_per_host",
        WorkerConfig::default().max_concurrent_agents_per_host as u64,
        "invalid_worker_max_concurrent_agents_per_host",
        "worker.max_concurrent_agents_per_host",
    )?;
    let max_concurrent_agents_per_host =
        usize::try_from(max_concurrent_agents_per_host).map_err(|_| {
            SymphonyError::config(
                "invalid_worker_max_concurrent_agents_per_host",
                "worker.max_concurrent_agents_per_host is too large",
            )
        })?;
    Ok(WorkerConfig {
        ssh_hosts: hosts,
        max_concurrent_agents_per_host,
    })
}

fn parse_workspace(config: &Mapping, workflow_dir: &Path) -> Result<WorkspaceConfig> {
    let workspace = get_map(config, "workspace");
    let configured_root = get_string(workspace, "root");
    let root = configured_root
        .as_deref()
        .map(resolve_path_value)
        .transpose()?
        .unwrap_or_else(|| env::temp_dir().join("symphony_workspaces"));
    let expanded = if root.is_absolute() {
        root
    } else {
        workflow_dir.join(root)
    };
    let remote_root = configured_root
        .filter(|root| root.starts_with('/'))
        .unwrap_or_else(|| expanded.to_string_lossy().replace('\\', "/"));
    Ok(WorkspaceConfig {
        root: normalize_absolute_path(&expanded)?,
        remote_root,
        cleanup: parse_workspace_cleanup(workspace)?,
        retention: parse_workspace_retention(workspace)?,
        population: parse_workspace_population(workspace)?,
    })
}

fn parse_workspace_cleanup(workspace: Option<&Mapping>) -> Result<WorkspaceCleanupConfig> {
    let cleanup = get_nested_map(workspace, "cleanup");
    let Some(after_success) = get_string(cleanup, "after_success") else {
        return Ok(WorkspaceCleanupConfig::default());
    };
    let after_success = match after_success.trim().to_ascii_lowercase().as_str() {
        "committed" => WorkspaceCleanupAfterSuccess::Committed,
        "never" => WorkspaceCleanupAfterSuccess::Never,
        _ => {
            return Err(SymphonyError::config(
                "invalid_workspace_cleanup_after_success",
                "workspace.cleanup.after_success must be committed or never",
            ));
        }
    };
    Ok(WorkspaceCleanupConfig { after_success })
}

fn parse_workspace_population(workspace: Option<&Mapping>) -> Result<WorkspacePopulationConfig> {
    let population = get_nested_map(workspace, "population");
    let Some(population) = population else {
        return Ok(WorkspacePopulationConfig::default());
    };

    let kind = match get_string(Some(population), "kind")
        .unwrap_or_else(|| "none".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "none" => WorkspacePopulationKind::None,
        "git" => WorkspacePopulationKind::Git,
        _ => {
            return Err(SymphonyError::config(
                "invalid_workspace_population_kind",
                "workspace.population.kind must be none or git",
            ));
        }
    };
    let repository_url =
        get_string(Some(population), "repository_url").map(|value| value.trim().to_string());
    let reference = get_string(Some(population), "ref").map(|value| value.trim().to_string());
    let branch = get_string(Some(population), "branch").map(|value| value.trim().to_string());
    let depth = get_i64(Some(population), "depth")
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    SymphonyError::config(
                        "invalid_workspace_population_depth",
                        "workspace.population.depth must be a positive integer",
                    )
                })
        })
        .transpose()?;
    let reuse = match get_string(Some(population), "reuse")
        .unwrap_or_else(|| "skip".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "skip" => WorkspacePopulationReusePolicy::Skip,
        "fetch_ff_only" => WorkspacePopulationReusePolicy::FetchFfOnly,
        _ => {
            return Err(SymphonyError::config(
                "invalid_workspace_population_reuse",
                "workspace.population.reuse must be skip or fetch_ff_only",
            ));
        }
    };

    if reference.is_some() && branch.is_some() {
        return Err(SymphonyError::config(
            "invalid_workspace_population_reference",
            "workspace.population.ref and workspace.population.branch cannot both be set",
        ));
    }
    match kind {
        WorkspacePopulationKind::None => {
            if repository_url.is_some()
                || reference.is_some()
                || branch.is_some()
                || depth.is_some()
            {
                return Err(SymphonyError::config(
                    "invalid_workspace_population_none",
                    "workspace.population settings require kind: git",
                ));
            }
        }
        WorkspacePopulationKind::Git => {
            if repository_url.as_deref().is_none_or(str::is_empty) {
                return Err(SymphonyError::config(
                    "missing_workspace_population_repository_url",
                    "workspace.population.repository_url must be non-empty when kind is git",
                ));
            }
            if reference.as_deref().is_some_and(str::is_empty)
                || branch.as_deref().is_some_and(str::is_empty)
            {
                return Err(SymphonyError::config(
                    "invalid_workspace_population_reference",
                    "workspace.population.ref and workspace.population.branch must be non-empty when set",
                ));
            }
            if reference
                .as_deref()
                .or(branch.as_deref())
                .is_some_and(|target| target.starts_with('-') || target.contains('\0'))
            {
                return Err(SymphonyError::config(
                    "invalid_workspace_population_reference",
                    "workspace.population.ref and workspace.population.branch must not start with '-' or contain NUL",
                ));
            }
        }
    }

    Ok(WorkspacePopulationConfig {
        kind,
        repository_url,
        reference,
        branch,
        depth,
        reuse,
    })
}

fn parse_workspace_retention(workspace: Option<&Mapping>) -> Result<WorkspaceRetentionConfig> {
    let Some(retention_value) = get_value(workspace, "retention") else {
        return Ok(WorkspaceRetentionConfig::default());
    };
    let retention = retention_value.as_mapping().ok_or_else(|| {
        SymphonyError::config(
            "invalid_workspace_retention",
            "workspace.retention must be a mapping",
        )
    })?;
    let Some(max_age_days) = get_value(Some(retention), "max_age_days") else {
        return Ok(WorkspaceRetentionConfig::default());
    };
    let max_age_days = match max_age_days {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse::<u64>().ok(),
        _ => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| {
        SymphonyError::config(
            "invalid_workspace_retention_max_age_days",
            "workspace.retention.max_age_days must be a positive integer",
        )
    })?;
    Ok(WorkspaceRetentionConfig {
        max_age_days: Some(max_age_days),
    })
}

fn parse_hooks(config: &Mapping) -> Result<HooksConfig> {
    let hooks = get_map(config, "hooks");
    let timeout_ms = positive_u64_or_default(
        hooks,
        "timeout_ms",
        60_000,
        "invalid_hooks_timeout_ms",
        "hooks.timeout_ms",
    )?;
    Ok(HooksConfig {
        after_create: get_string(hooks, "after_create"),
        before_run: get_string(hooks, "before_run"),
        after_run: get_string(hooks, "after_run"),
        before_remove: get_string(hooks, "before_remove"),
        timeout_ms,
    })
}

fn parse_agent(config: &Mapping) -> Result<AgentConfig> {
    let agent = get_map(config, "agent");
    let max_concurrent_agents = get_i64(agent, "max_concurrent_agents").unwrap_or(10);
    let max_turns = positive_u64_or_default(
        agent,
        "max_turns",
        20,
        "invalid_max_turns",
        "agent.max_turns",
    )?;
    let max_turns = u32::try_from(max_turns).map_err(|_| {
        SymphonyError::config(
            "invalid_max_turns",
            "agent.max_turns must be a positive 32-bit integer",
        )
    })?;
    let max_retry_backoff_ms = get_i64(agent, "max_retry_backoff_ms").unwrap_or(300_000);
    if max_concurrent_agents <= 0 {
        return Err(SymphonyError::config(
            "invalid_max_concurrent_agents",
            "agent.max_concurrent_agents must be positive",
        ));
    }
    if max_retry_backoff_ms <= 0 {
        return Err(SymphonyError::config(
            "invalid_max_retry_backoff_ms",
            "agent.max_retry_backoff_ms must be positive",
        ));
    }
    Ok(AgentConfig {
        max_concurrent_agents: max_concurrent_agents as usize,
        max_turns,
        max_retry_backoff_ms: max_retry_backoff_ms as u64,
        max_concurrent_agents_by_state: get_positive_usize_map(
            agent,
            "max_concurrent_agents_by_state",
        ),
    })
}

fn parse_codex(config: &Mapping) -> Result<CodexConfig> {
    let codex = get_map(config, "codex");
    let command = get_string(codex, "command").unwrap_or_else(|| "codex app-server".to_string());
    let turn_timeout_ms = get_i64(codex, "turn_timeout_ms").unwrap_or(3_600_000);
    let read_timeout_ms = get_i64(codex, "read_timeout_ms").unwrap_or(5_000);
    let stall_timeout_ms = get_i64(codex, "stall_timeout_ms").unwrap_or(300_000);
    if turn_timeout_ms <= 0 {
        return Err(SymphonyError::config(
            "invalid_turn_timeout_ms",
            "codex.turn_timeout_ms must be positive",
        ));
    }
    if read_timeout_ms <= 0 {
        return Err(SymphonyError::config(
            "invalid_read_timeout_ms",
            "codex.read_timeout_ms must be positive",
        ));
    }
    let trusted_danger_full_access = get_bool(codex, "trusted_danger_full_access").unwrap_or(false);
    let thread_sandbox = get_json_value(codex, "thread_sandbox")
        .or_else(|| Some(serde_json::json!("workspace-write")));
    let turn_sandbox_policy = get_json_value(codex, "turn_sandbox_policy");
    if !trusted_danger_full_access
        && thread_sandbox.as_ref().and_then(serde_json::Value::as_str) == Some("danger-full-access")
    {
        return Err(SymphonyError::config(
            "untrusted_danger_full_access",
            "codex.thread_sandbox: danger-full-access requires codex.trusted_danger_full_access: true",
        ));
    }
    if !trusted_danger_full_access
        && turn_sandbox_policy
            .as_ref()
            .and_then(|policy| policy.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("dangerFullAccess")
    {
        return Err(SymphonyError::config(
            "untrusted_danger_full_access",
            "codex.turn_sandbox_policy.type: dangerFullAccess requires codex.trusted_danger_full_access: true",
        ));
    }
    Ok(CodexConfig {
        command,
        approval_policy: get_json_value(codex, "approval_policy")
            .or_else(|| Some(serde_json::json!("never"))),
        trusted_danger_full_access,
        thread_sandbox,
        turn_sandbox_policy,
        turn_timeout_ms: turn_timeout_ms as u64,
        read_timeout_ms: read_timeout_ms as u64,
        stall_timeout_ms,
    })
}

fn parse_completion(config: &Mapping) -> Result<CompletionConfig> {
    let completion = get_map(config, "completion");
    let direct_commit = get_nested_map(completion, "direct_commit");
    let started_state = get_string(direct_commit, "started_state")
        .or_else(|| get_string(completion, "started_state"))
        .map(|state| state.trim().to_string());
    let mut direct_commit_config = DirectCommitCompletionConfig {
        enabled: get_bool(direct_commit, "enabled").unwrap_or(false),
        dry_run: get_bool(direct_commit, "dry_run").unwrap_or(false),
        base_branch: get_string(direct_commit, "base_branch").unwrap_or_else(|| "main".to_string()),
        high_review_state: get_string(direct_commit, "high_review_state")
            .or_else(|| get_string(completion, "high_review_state"))
            .unwrap_or_else(|| "In review".to_string()),
        auto_approved_state: get_string(direct_commit, "auto_approved_state")
            .or_else(|| get_string(completion, "auto_approved_state"))
            .unwrap_or_else(|| "Done".to_string()),
        started_state,
        commit_author_name: get_string(direct_commit, "commit_author_name")
            .unwrap_or_else(|| "Symphony".to_string()),
        commit_author_email: get_string(direct_commit, "commit_author_email")
            .unwrap_or_else(|| "symphony@users.noreply.github.com".to_string()),
    };
    direct_commit_config.base_branch = direct_commit_config.base_branch.trim().to_string();
    direct_commit_config.high_review_state =
        direct_commit_config.high_review_state.trim().to_string();
    direct_commit_config.auto_approved_state =
        direct_commit_config.auto_approved_state.trim().to_string();
    direct_commit_config.commit_author_name =
        direct_commit_config.commit_author_name.trim().to_string();
    direct_commit_config.commit_author_email =
        direct_commit_config.commit_author_email.trim().to_string();
    if direct_commit_config.enabled {
        if direct_commit_config.base_branch.is_empty() {
            return Err(SymphonyError::config(
                "invalid_completion_base_branch",
                "completion.direct_commit.base_branch must be non-empty",
            ));
        }
        if direct_commit_config.high_review_state.is_empty() {
            return Err(SymphonyError::config(
                "invalid_completion_high_review_state",
                "completion.direct_commit.high_review_state must be non-empty",
            ));
        }
        if direct_commit_config.auto_approved_state.is_empty() {
            return Err(SymphonyError::config(
                "invalid_completion_auto_approved_state",
                "completion.direct_commit.auto_approved_state must be non-empty",
            ));
        }
        if direct_commit_config
            .started_state
            .as_deref()
            .is_some_and(str::is_empty)
        {
            return Err(SymphonyError::config(
                "invalid_completion_started_state",
                "completion.direct_commit.started_state must be non-empty when configured",
            ));
        }
        if direct_commit_config.commit_author_name.is_empty() {
            return Err(SymphonyError::config(
                "invalid_completion_commit_author_name",
                "completion.direct_commit.commit_author_name must be non-empty",
            ));
        }
        if direct_commit_config.commit_author_email.is_empty() {
            return Err(SymphonyError::config(
                "invalid_completion_commit_author_email",
                "completion.direct_commit.commit_author_email must be non-empty",
            ));
        }
    }
    Ok(CompletionConfig {
        direct_commit: direct_commit_config,
    })
}

fn parse_server(config: &Mapping) -> Result<ServerConfig> {
    let Some(server) = get_map(config, "server") else {
        return Ok(ServerConfig::default());
    };

    let host = match get_value(Some(server), "host") {
        Some(Value::String(value)) => value.trim().parse::<IpAddr>().map_err(|_| {
            SymphonyError::config("invalid_server_host", "server.host must be an IP address")
        })?,
        Some(_) => {
            return Err(SymphonyError::config(
                "invalid_server_host",
                "server.host must be an IP address",
            ));
        }
        None => ServerConfig::default().host,
    };
    let port = match get_value(Some(server), "port") {
        Some(value) => {
            let Some(port) = value.as_i64() else {
                return Err(SymphonyError::config(
                    "invalid_server_port",
                    "server.port must be an integer between 0 and 65535",
                ));
            };
            if !(0..=65_535).contains(&port) {
                return Err(SymphonyError::config(
                    "invalid_server_port",
                    "server.port must be an integer between 0 and 65535",
                ));
            }
            Some(port as u16)
        }
        None => None,
    };
    let auth_token =
        get_string(Some(server), "auth_token").and_then(|value| resolve_secret(&value));
    if !host.is_loopback() && auth_token.is_none() {
        return Err(SymphonyError::config(
            "missing_server_auth_token",
            "server.auth_token is required when server.host is non-loopback",
        ));
    }
    let refresh_cooldown_ms = positive_u64_or_default(
        Some(server),
        "refresh_cooldown_ms",
        ServerConfig::default().refresh_cooldown_ms,
        "invalid_server_refresh_cooldown_ms",
        "server.refresh_cooldown_ms",
    )?;
    let drain_timeout_ms = positive_u64_or_default(
        Some(server),
        "drain_timeout_ms",
        ServerConfig::default().drain_timeout_ms,
        "invalid_server_drain_timeout_ms",
        "server.drain_timeout_ms",
    )?;
    Ok(ServerConfig {
        host,
        port,
        auth_token,
        refresh_cooldown_ms,
        drain_timeout_ms,
    })
}

fn tracker_api_key_diagnostic(config: &Mapping) -> TrackerApiKeyDiagnostic {
    let tracker = get_map(config, "tracker");
    let kind = get_string(tracker, "kind")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let raw_api_key = get_string(tracker, "api_key")
        .or_else(|| get_string(tracker, "token"))
        .or_else(|| (kind == "github").then(|| "$GITHUB_TOKEN".to_string()));
    let Some(raw_api_key) = raw_api_key else {
        return TrackerApiKeyDiagnostic {
            source: TrackerApiKeySource::Missing,
            presence: EnvironmentValuePresence::Missing,
        };
    };
    if let Some(variable) = environment_reference(&raw_api_key) {
        return TrackerApiKeyDiagnostic {
            source: TrackerApiKeySource::Environment {
                variable: variable.to_string(),
            },
            presence: environment_value_presence(variable),
        };
    }
    TrackerApiKeyDiagnostic {
        source: TrackerApiKeySource::Literal,
        presence: if raw_api_key.is_empty() {
            EnvironmentValuePresence::Empty
        } else {
            EnvironmentValuePresence::Present
        },
    }
}

fn default_workspace_root_diagnostic() -> WorkspaceRootDiagnostic {
    let root = env::temp_dir().join("symphony_workspaces");
    match normalize_absolute_path(&root) {
        Ok(normalized_path) => WorkspaceRootDiagnostic {
            source: WorkspaceRootSource::Default,
            environment_presence: None,
            status: ConfigDiagnosticCheck::passed(),
            normalized_path: Some(normalized_path),
        },
        Err(error) => WorkspaceRootDiagnostic {
            source: WorkspaceRootSource::Default,
            environment_presence: None,
            status: ConfigDiagnosticCheck::failed(&error),
            normalized_path: None,
        },
    }
}

fn workspace_root_diagnostic(
    config: &Mapping,
    workflow_dir: Option<&Path>,
) -> WorkspaceRootDiagnostic {
    let root = get_string(get_map(config, "workspace"), "root");
    let (source, environment_presence, root) = match root {
        Some(value) => match environment_reference(&value) {
            Some(variable) => (
                WorkspaceRootSource::Environment {
                    variable: variable.to_string(),
                },
                Some(environment_value_presence(variable)),
                resolve_path_value(&value),
            ),
            None => (
                WorkspaceRootSource::Literal,
                None,
                resolve_path_value(&value),
            ),
        },
        None => return default_workspace_root_diagnostic(),
    };
    let root = match root {
        Ok(root) => root,
        Err(error) => {
            return WorkspaceRootDiagnostic {
                source,
                environment_presence,
                status: ConfigDiagnosticCheck::failed(&error),
                normalized_path: None,
            };
        }
    };
    let workflow_dir = workflow_dir.unwrap_or_else(|| Path::new("."));
    let expanded = if root.is_absolute() {
        root
    } else {
        workflow_dir.join(root)
    };
    match normalize_absolute_path(&expanded) {
        Ok(normalized_path) => WorkspaceRootDiagnostic {
            source,
            environment_presence,
            status: ConfigDiagnosticCheck::passed(),
            normalized_path: Some(normalized_path),
        },
        Err(error) => WorkspaceRootDiagnostic {
            source,
            environment_presence,
            status: ConfigDiagnosticCheck::failed(&error),
            normalized_path: None,
        },
    }
}

fn environment_reference(value: &str) -> Option<&str> {
    let name = value.strip_prefix('$')?;
    (!name.is_empty() && !name.contains(['/', ' ', '$'])).then_some(name)
}

fn environment_value_presence(name: &str) -> EnvironmentValuePresence {
    match env::var(name) {
        Ok(value) if value.is_empty() => EnvironmentValuePresence::Empty,
        Ok(_) => EnvironmentValuePresence::Present,
        Err(env::VarError::NotPresent) => EnvironmentValuePresence::Missing,
        Err(env::VarError::NotUnicode(_)) => EnvironmentValuePresence::Empty,
    }
}

pub fn normalize_state(state: &str) -> String {
    state.to_ascii_lowercase()
}

pub fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| SymphonyError::io(None, err))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn resolve_secret(value: &str) -> Option<String> {
    let resolved = match environment_reference(value) {
        Some(name) => env::var(name).unwrap_or_default(),
        None => value.to_string(),
    };
    (!resolved.is_empty()).then_some(resolved)
}

fn resolve_path_value(value: &str) -> Result<PathBuf> {
    let resolved = match environment_reference(value) {
        Some(name) => env::var(name).map_err(|_| {
            SymphonyError::config(
                "missing_path_env",
                format!("environment variable {name} referenced by path is missing"),
            )
        })?,
        None => value.to_string(),
    };
    let expanded = if resolved == "~" {
        home_dir()?.to_string_lossy().to_string()
    } else if let Some(rest) = resolved.strip_prefix("~/") {
        home_dir()?.join(rest).to_string_lossy().to_string()
    } else {
        resolved
    };
    Ok(PathBuf::from(expanded))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| SymphonyError::config("missing_home", "HOME is required for ~ expansion"))
}

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

fn get_map<'a>(mapping: &'a Mapping, name: &str) -> Option<&'a Mapping> {
    mapping.get(key(name)).and_then(Value::as_mapping)
}

fn get_nested_map<'a>(mapping: Option<&'a Mapping>, name: &str) -> Option<&'a Mapping> {
    mapping.and_then(|m| get_map(m, name))
}

fn get_value<'a>(mapping: Option<&'a Mapping>, name: &str) -> Option<&'a Value> {
    mapping.and_then(|m| m.get(key(name)))
}

fn get_string(mapping: Option<&Mapping>, name: &str) -> Option<String> {
    get_value(mapping, name).and_then(|value| match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn get_i64(mapping: Option<&Mapping>, name: &str) -> Option<i64> {
    get_value(mapping, name).and_then(|value| match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    })
}

fn positive_u64_or_default(
    mapping: Option<&Mapping>,
    name: &str,
    default: u64,
    code: &'static str,
    field: &'static str,
) -> Result<u64> {
    let Some(value) = get_value(mapping, name) else {
        return Ok(default);
    };
    let value = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.parse::<u64>().ok(),
        _ => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| SymphonyError::config(code, format!("{field} must be a positive integer")))?;
    Ok(value)
}

fn get_bool(mapping: Option<&Mapping>, name: &str) -> Option<bool> {
    get_value(mapping, name).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::String(value) => value.parse::<bool>().ok(),
        _ => None,
    })
}

fn get_string_list(mapping: Option<&Mapping>, name: &str) -> Option<Vec<String>> {
    get_value(mapping, name).and_then(|value| match value {
        Value::Sequence(values) => Some(
            values
                .iter()
                .filter_map(|value| match value {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
        ),
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    })
}

fn get_i64_map(mapping: Option<&Mapping>, name: &str) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    if let Some(Value::Mapping(values)) = get_value(mapping, name) {
        for (key, value) in values {
            let Some(key) = key.as_str() else { continue };
            let Some(value) = value.as_i64() else {
                continue;
            };
            out.insert(key.to_ascii_lowercase(), value);
        }
    }
    out
}

fn get_positive_usize_map(mapping: Option<&Mapping>, name: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    if let Some(Value::Mapping(values)) = get_value(mapping, name) {
        for (key, value) in values {
            let Some(key) = key.as_str() else { continue };
            let value = match value {
                Value::Number(number) => number.as_i64(),
                Value::String(string) => string.parse::<i64>().ok(),
                _ => None,
            };
            let Some(value) = value else { continue };
            if value > 0 {
                out.insert(normalize_state(key), value as usize);
            }
        }
    }
    out
}

fn get_json_value(mapping: Option<&Mapping>, name: &str) -> Option<serde_json::Value> {
    get_value(mapping, name).and_then(|value| serde_json::to_value(value).ok())
}
