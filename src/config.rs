//! Typed workflow configuration.
//!
//! Implementation-defined choices documented here:
//! - Tracker adapter: this Rust implementation supports `tracker.kind: github` and maps GitHub
//!   Projects v2 Status values onto Symphony issue states. Repository-only issues are not dispatched.
//! - Approval/sandbox posture: default Codex policy is high-trust (`approvalPolicy = "never"`,
//!   thread sandbox `danger-full-access`, turn sandbox policy `{type: "dangerFullAccess"}`). Workflows
//!   may override these pass-through values with schema-valid Codex values.
//! - Workspace population: Symphony only creates/reuses per-issue directories. Checkout/sync/bootstrap
//!   is owned by configured hooks.
//! - Logging sink: structured logs are emitted to stderr.
//! - GitHub endpoint policy: workflow-supplied `tracker.endpoint` values are ignored for
//!   `tracker.kind: github`; this implementation always uses the public GitHub GraphQL endpoint.
//! - Existing non-directory workspace path policy: fail safely; never replace user data.
//! - User-input-required policy: the Codex client fails the run rather than waiting indefinitely.
//! - Container runtime: the published image uses an init/reaper, executes hooks and Codex inside the
//!   container namespace, and expects workflow/workspace paths to be container paths.

use std::collections::BTreeMap;
use std::env;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::domain::WorkflowDefinition;
use crate::error::{Result, SymphonyError};
use crate::workflow::{load_workflow, select_workflow_path};

pub const DEFAULT_GITHUB_ENDPOINT: &str = "https://api.github.com/graphql";
pub const DEFAULT_PROMPT: &str = "You are working on an issue from GitHub.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfig {
    pub workflow_path: PathBuf,
    pub workflow_dir: PathBuf,
    pub prompt_template: String,
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub hooks: HooksConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub completion: CompletionConfig,
}

impl EffectiveConfig {
    pub fn from_workflow(workflow: WorkflowDefinition) -> Result<Self> {
        let workflow_dir = workflow
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let tracker = parse_tracker(&workflow.config)?;
        let polling = parse_polling(&workflow.config)?;
        let workspace = parse_workspace(&workflow.config, &workflow_dir)?;
        let hooks = parse_hooks(&workflow.config)?;
        let agent = parse_agent(&workflow.config)?;
        let codex = parse_codex(&workflow.config)?;
        let completion = parse_completion(&workflow.config)?;
        Ok(Self {
            workflow_path: workflow.path,
            workflow_dir,
            prompt_template: workflow.prompt_template,
            tracker,
            polling,
            workspace,
            hooks,
            agent,
            codex,
            completion,
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
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub github: Option<GithubConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubConfig {
    pub repository_owner: String,
    pub repository_name: String,
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
pub struct WorkspaceConfig {
    pub root: PathBuf,
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
    pub thread_sandbox: Option<serde_json::Value>,
    pub turn_sandbox_policy: Option<serde_json::Value>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionConfig {
    pub direct_commit: DirectCommitCompletionConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCommitCompletionConfig {
    pub enabled: bool,
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

    pub fn reload_now(&mut self) -> Result<&EffectiveConfig> {
        let next = EffectiveConfig::from_workflow(load_workflow(&self.path)?)?;
        next.validate_dispatch()?;
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

fn modified_time(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

fn parse_tracker(config: &Mapping) -> Result<TrackerConfig> {
    let tracker = get_map(config, "tracker");
    let kind = get_string(tracker, "kind")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let endpoint = if kind == "github" {
        DEFAULT_GITHUB_ENDPOINT.to_string()
    } else {
        get_string(tracker, "endpoint").unwrap_or_default()
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
        api_key,
        active_states,
        terminal_states,
        github,
    })
}

fn parse_github_config(tracker: Option<&Mapping>) -> Result<GithubConfig> {
    let repository = get_nested_map(tracker, "repository");
    let project = get_nested_map(tracker, "project");
    let repository_owner = get_string(repository, "owner")
        .or_else(|| get_string(tracker, "repository_owner"))
        .unwrap_or_default();
    let repository_name = get_string(repository, "name")
        .or_else(|| get_string(tracker, "repository_name"))
        .unwrap_or_default();
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

fn parse_workspace(config: &Mapping, workflow_dir: &Path) -> Result<WorkspaceConfig> {
    let workspace = get_map(config, "workspace");
    let root = get_string(workspace, "root")
        .map(|value| resolve_path_value(&value))
        .transpose()?
        .unwrap_or_else(|| env::temp_dir().join("symphony_workspaces"));
    let expanded = if root.is_absolute() {
        root
    } else {
        workflow_dir.join(root)
    };
    Ok(WorkspaceConfig {
        root: normalize_absolute_path(&expanded)?,
    })
}

fn parse_hooks(config: &Mapping) -> Result<HooksConfig> {
    let hooks = get_map(config, "hooks");
    let timeout_ms = get_i64(hooks, "timeout_ms").unwrap_or(60_000);
    if timeout_ms <= 0 {
        return Err(SymphonyError::config(
            "invalid_hooks_timeout_ms",
            "hooks.timeout_ms must be positive",
        ));
    }
    Ok(HooksConfig {
        after_create: get_string(hooks, "after_create"),
        before_run: get_string(hooks, "before_run"),
        after_run: get_string(hooks, "after_run"),
        before_remove: get_string(hooks, "before_remove"),
        timeout_ms: timeout_ms as u64,
    })
}

fn parse_agent(config: &Mapping) -> Result<AgentConfig> {
    let agent = get_map(config, "agent");
    let max_concurrent_agents = get_i64(agent, "max_concurrent_agents").unwrap_or(10);
    let max_turns = get_i64(agent, "max_turns").unwrap_or(20);
    let max_retry_backoff_ms = get_i64(agent, "max_retry_backoff_ms").unwrap_or(300_000);
    if max_concurrent_agents <= 0 {
        return Err(SymphonyError::config(
            "invalid_max_concurrent_agents",
            "agent.max_concurrent_agents must be positive",
        ));
    }
    if max_turns <= 0 {
        return Err(SymphonyError::config(
            "invalid_max_turns",
            "agent.max_turns must be positive",
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
        max_turns: max_turns as u32,
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
    Ok(CodexConfig {
        command,
        approval_policy: get_json_value(codex, "approval_policy")
            .or_else(|| Some(serde_json::json!("never"))),
        thread_sandbox: get_json_value(codex, "thread_sandbox")
            .or_else(|| Some(serde_json::json!("danger-full-access"))),
        turn_sandbox_policy: get_json_value(codex, "turn_sandbox_policy")
            .or_else(|| Some(serde_json::json!({ "type": "dangerFullAccess" }))),
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
    let resolved = if let Some(name) = value.strip_prefix('$') {
        if name.is_empty() || name.contains(['/', ' ', '$']) {
            value.to_string()
        } else {
            env::var(name).unwrap_or_default()
        }
    } else {
        value.to_string()
    };
    (!resolved.is_empty()).then_some(resolved)
}

fn resolve_path_value(value: &str) -> Result<PathBuf> {
    let resolved = if let Some(name) = value.strip_prefix('$') {
        if !name.is_empty() && !name.contains(['/', ' ', '$']) {
            env::var(name).map_err(|_| {
                SymphonyError::config(
                    "missing_path_env",
                    format!("environment variable {name} referenced by path is missing"),
                )
            })?
        } else {
            value.to_string()
        }
    } else {
        value.to_string()
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
