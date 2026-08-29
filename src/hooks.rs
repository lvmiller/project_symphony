use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

use crate::config::{
    HooksConfig, WorkspacePopulationConfig, WorkspacePopulationKind, WorkspacePopulationReusePolicy,
};
use crate::error::{Result, SymphonyError};

const MAX_HOOK_OUTPUT_BYTES: usize = 8 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct HookContext {
    workspace_path: PathBuf,
    workspace_key: String,
    source_id: String,
    issue_identifier: String,
}

impl HookContext {
    pub fn new(
        workspace_path: &Path,
        workspace_key: String,
        source_id: impl Into<String>,
        issue_identifier: impl Into<String>,
    ) -> Self {
        Self {
            workspace_path: workspace_path.to_path_buf(),
            workspace_key,
            source_id: source_id.into(),
            issue_identifier: issue_identifier.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    AfterCreate,
    BeforeRun,
    AfterRun,
    BeforeRemove,
}

impl HookKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::AfterCreate => "after_create",
            Self::BeforeRun => "before_run",
            Self::AfterRun => "after_run",
            Self::BeforeRemove => "before_remove",
        }
    }

    fn script(self, hooks: &HooksConfig) -> Option<&str> {
        match self {
            Self::AfterCreate => hooks.after_create.as_deref(),
            Self::BeforeRun => hooks.before_run.as_deref(),
            Self::AfterRun => hooks.after_run.as_deref(),
            Self::BeforeRemove => hooks.before_remove.as_deref(),
        }
    }
}

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    fn append(&mut self, chunk: &[u8]) {
        let available = MAX_HOOK_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let retained = available.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated |= retained < chunk.len();
    }

    fn excerpt(&self) -> String {
        let redacted = redact_hook_output(&String::from_utf8_lossy(&self.bytes));
        if self.truncated {
            format!("{redacted} [truncated]")
        } else {
            redacted
        }
    }
}

pub async fn run_hook(kind: HookKind, hooks: &HooksConfig, context: HookContext) -> Result<()> {
    let Some(script) = kind.script(hooks) else {
        return Ok(());
    };
    info!(
        hook = kind.name(),
        workspace = %context.workspace_path.display(),
        workspace_key = %context.workspace_key,
        source_key = %crate::workspace::source_workspace_namespace(&context.source_id),
        issue_key = %crate::workspace::sanitize_workspace_key(&context.issue_identifier),
        "hook started"
    );
    let started_at = Instant::now();
    let mut child = Command::new("sh")
        .arg("-lc")
        .arg(script)
        .current_dir(&context.workspace_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SYMPHONY_HOOK_NAME", kind.name())
        .env("SYMPHONY_WORKSPACE_PATH", &context.workspace_path)
        .env("SYMPHONY_WORKSPACE_KEY", &context.workspace_key)
        .env("SYMPHONY_SOURCE_ID", &context.source_id)
        .env(
            "SYMPHONY_SOURCE_KEY",
            crate::workspace::source_workspace_namespace(&context.source_id),
        )
        .env("SYMPHONY_ISSUE_IDENTIFIER", &context.issue_identifier)
        .env(
            "SYMPHONY_ISSUE_KEY",
            crate::workspace::sanitize_workspace_key(&context.issue_identifier),
        )
        .spawn()
        .map_err(|err| {
            hook_error(
                kind,
                &context,
                started_at,
                format!("spawn_failed={err}"),
                None,
                None,
            )
        })?;

    let stdout = Arc::new(Mutex::new(CapturedOutput::default()));
    let stderr = Arc::new(Mutex::new(CapturedOutput::default()));
    let mut stdout_task = spawn_output_drain(
        child.stdout.take().expect("hook stdout must be piped"),
        Arc::clone(&stdout),
    );
    let mut stderr_task = spawn_output_drain(
        child.stderr.take().expect("hook stderr must be piped"),
        Arc::clone(&stderr),
    );

    let completion = match timeout(Duration::from_millis(hooks.timeout_ms), child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(format!("wait_failed={error}")),
        Err(_) => {
            let _ = child.start_kill();
            match child.wait().await {
                Ok(_) => Err(format!("timeout after {} ms", hooks.timeout_ms)),
                Err(error) => Err(format!(
                    "timeout after {} ms; reap_failed={error}",
                    hooks.timeout_ms
                )),
            }
        }
    };

    drain_or_abort(&mut stdout_task).await;
    drain_or_abort(&mut stderr_task).await;

    let stdout = stdout
        .lock()
        .expect("hook stdout capture lock poisoned")
        .excerpt();
    let stderr = stderr
        .lock()
        .expect("hook stderr capture lock poisoned")
        .excerpt();
    match completion {
        Ok(status) if status.success() => {
            info!(
                hook = kind.name(),
                workspace = %context.workspace_path.display(),
                elapsed_ms = started_at.elapsed().as_millis(),
                "hook completed"
            );
            Ok(())
        }
        Ok(status) => Err(hook_error(
            kind,
            &context,
            started_at,
            format!("exit_status={status}"),
            Some(stdout),
            Some(stderr),
        )),
        Err(status) => Err(hook_error(
            kind,
            &context,
            started_at,
            status,
            Some(stdout),
            Some(stderr),
        )),
    }
}

pub async fn run_hook_best_effort(kind: HookKind, hooks: &HooksConfig, context: HookContext) {
    if let Err(error) = run_hook(kind, hooks, context).await {
        warn!(hook = kind.name(), error = %error, "hook failed ignored");
    }
}

fn spawn_output_drain<R>(reader: R, output: Arc<Mutex<CapturedOutput>>) -> JoinHandle<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => return,
                Ok(read) => output
                    .lock()
                    .expect("hook output capture lock poisoned")
                    .append(&chunk[..read]),
                Err(_) => return,
            }
        }
    })
}

async fn drain_or_abort(task: &mut JoinHandle<()>) {
    if timeout(OUTPUT_DRAIN_GRACE, &mut *task).await.is_err() {
        task.abort();
        let _ = (&mut *task).await;
    }
}

fn hook_error(
    kind: HookKind,
    context: &HookContext,
    started_at: Instant,
    status: String,
    stdout: Option<String>,
    stderr: Option<String>,
) -> SymphonyError {
    let mut message = format!(
        "{status} elapsed_ms={} workspace={} workspace_key={} source_key={} issue_key={}",
        started_at.elapsed().as_millis(),
        context.workspace_path.display(),
        context.workspace_key,
        crate::workspace::source_workspace_namespace(&context.source_id),
        crate::workspace::sanitize_workspace_key(&context.issue_identifier),
    );
    if let Some(stdout) = stdout {
        message.push_str(&format!(" stdout_excerpt={stdout:?}"));
    }
    if let Some(stderr) = stderr {
        message.push_str(&format!(" stderr_excerpt={stderr:?}"));
    }
    SymphonyError::Hook {
        hook: kind.name(),
        message,
    }
}

fn redact_hook_output(output: &str) -> String {
    let mut redacted = output.to_string();
    let mut environment_secrets: Vec<String> = std::env::vars()
        .filter_map(|(name, value)| {
            let name = name.to_ascii_uppercase();
            (name.contains("TOKEN")
                || name.contains("SECRET")
                || name.contains("PASSWORD")
                || name.contains("API_KEY")
                || name.contains("AUTH")
                || name.contains("CREDENTIAL")
                || name.contains("PRIVATE_KEY"))
            .then_some(value)
        })
        .filter(|value| value.len() >= 4)
        .collect();
    environment_secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    environment_secrets.dedup();
    for secret in environment_secrets {
        redacted = redacted.replace(&secret, "[REDACTED]");
    }
    redact_hook_url_credentials(&redact_labeled_secrets(&redacted))
}

fn redact_labeled_secrets(output: &str) -> String {
    const LABELS: [&str; 6] = [
        "authorization",
        "api_key",
        "password",
        "secret",
        "token",
        "credential",
    ];
    let mut redacted = output.to_string();
    for label in LABELS {
        let mut search_from = 0;
        loop {
            let lowercase = redacted[search_from..].to_ascii_lowercase();
            let Some(relative_start) = lowercase.find(label) else {
                break;
            };
            let label_start = search_from + relative_start;
            let mut value_start = label_start + label.len();
            while redacted[value_start..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_whitespace())
            {
                value_start += 1;
            }
            if !matches!(redacted[value_start..].chars().next(), Some('=' | ':')) {
                search_from = value_start;
                continue;
            }
            value_start += 1;
            while redacted[value_start..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_whitespace())
            {
                value_start += 1;
            }
            if redacted[value_start..]
                .get(..6)
                .is_some_and(|value| value.eq_ignore_ascii_case("bearer"))
                && redacted[value_start + 6..]
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_whitespace())
            {
                value_start += 6;
                while redacted[value_start..]
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_whitespace())
                {
                    value_start += 1;
                }
            }
            let value_end = redacted[value_start..]
                .find(|character: char| {
                    character.is_ascii_whitespace()
                        || matches!(character, '"' | '\'' | ',' | ';' | ')')
                })
                .map_or(redacted.len(), |offset| value_start + offset);
            if value_end == value_start {
                search_from = value_start;
                continue;
            }
            redacted.replace_range(value_start..value_end, "[REDACTED]");
            search_from = value_start + "[REDACTED]".len();
        }
    }
    redact_github_tokens(&redacted)
}

fn redact_github_tokens(output: &str) -> String {
    const PREFIXES: [&str; 5] = ["github_pat_", "ghp_", "gho_", "ghs_", "ghr_"];
    let mut redacted = output.to_string();
    for prefix in PREFIXES {
        let mut search_from = 0;
        while let Some(relative_start) = redacted[search_from..].find(prefix) {
            let start = search_from + relative_start;
            let end = redacted[start..]
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
                })
                .map_or(redacted.len(), |offset| start + offset);
            redacted.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }
    redacted
}

fn redact_hook_url_credentials(output: &str) -> String {
    let mut redacted = String::with_capacity(output.len());
    let mut remaining = output;
    while let Some(scheme_offset) = remaining.find("://") {
        let credentials_start = scheme_offset + 3;
        let authority_end = remaining[credentials_start..]
            .find(|character: char| {
                character.is_ascii_whitespace() || matches!(character, '/' | '?')
            })
            .map_or(remaining.len(), |offset| credentials_start + offset);
        let authority = &remaining[credentials_start..authority_end];
        let Some(at_offset) = authority.rfind('@') else {
            let split_at = authority_end;
            redacted.push_str(&remaining[..split_at]);
            remaining = &remaining[split_at..];
            continue;
        };
        redacted.push_str(&remaining[..credentials_start]);
        redacted.push_str("[REDACTED]@");
        redacted.push_str(&authority[at_offset + 1..]);
        remaining = &remaining[authority_end..];
    }
    redacted.push_str(remaining);
    redacted
}

/// Populates a workspace before its lifecycle hooks run.
///
/// Git operations use fixed argument vectors and never execute through a shell. A reused
/// workspace is left entirely alone unless the explicitly opt-in fast-forward-only policy is
/// configured.
pub(crate) async fn populate_workspace(
    population: &WorkspacePopulationConfig,
    workspace: &Path,
    created_now: bool,
) -> Result<()> {
    match population.kind {
        WorkspacePopulationKind::None => Ok(()),
        WorkspacePopulationKind::Git if created_now => clone_workspace(population, workspace).await,
        WorkspacePopulationKind::Git
            if matches!(
                population.reuse,
                WorkspacePopulationReusePolicy::FetchFfOnly
            ) =>
        {
            sync_workspace(population, workspace).await
        }
        WorkspacePopulationKind::Git => Ok(()),
    }
}

async fn clone_workspace(population: &WorkspacePopulationConfig, workspace: &Path) -> Result<()> {
    let repository_url = required_repository_url(population)?;
    let mut arguments = vec!["clone".to_string()];
    if let Some(depth) = population.depth {
        arguments.push("--depth".to_string());
        arguments.push(depth.to_string());
    }
    if let Some(branch) = &population.branch {
        arguments.push("--branch".to_string());
        arguments.push(branch.clone());
    }
    arguments.push("--".to_string());
    arguments.push(repository_url.to_string());
    arguments.push(".".to_string());
    run_git(workspace, "clone", repository_url, &arguments).await?;

    if let Some(reference) = &population.reference {
        run_git(
            workspace,
            "checkout configured ref",
            repository_url,
            &[
                "checkout".to_string(),
                "--detach".to_string(),
                reference.clone(),
            ],
        )
        .await?;
    }
    Ok(())
}

async fn sync_workspace(population: &WorkspacePopulationConfig, workspace: &Path) -> Result<()> {
    let repository_url = required_repository_url(population)?;
    let mut fetch_arguments = vec![
        "fetch".to_string(),
        "--no-tags".to_string(),
        "origin".to_string(),
    ];
    if let Some(branch) = &population.branch {
        fetch_arguments.push(branch.clone());
    } else if let Some(reference) = &population.reference {
        fetch_arguments.push(reference.clone());
    }
    run_git(workspace, "fetch", repository_url, &fetch_arguments).await?;

    let merge_target = if population.branch.is_some() || population.reference.is_some() {
        "FETCH_HEAD"
    } else {
        "@{upstream}"
    };
    run_git(
        workspace,
        "fast-forward sync",
        repository_url,
        &[
            "merge".to_string(),
            "--ff-only".to_string(),
            merge_target.to_string(),
        ],
    )
    .await
}

fn required_repository_url(population: &WorkspacePopulationConfig) -> Result<&str> {
    population.repository_url.as_deref().ok_or_else(|| {
        SymphonyError::Workspace("git workspace population is missing repository_url".to_string())
    })
}

async fn run_git(
    workspace: &Path,
    operation: &'static str,
    repository_url: &str,
    arguments: &[String],
) -> Result<()> {
    info!(operation, workspace = %workspace.display(), "workspace git population started");
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| {
            SymphonyError::Workspace(format!("git {operation} failed to start: {error}"))
        })?;

    if output.status.success() {
        info!(operation, workspace = %workspace.display(), "workspace git population completed");
        return Ok(());
    }

    let status = output.status;
    let output = String::from_utf8_lossy(&output.stderr);
    let output = redact_repository_url(&output, repository_url);
    let detail = output.trim();
    let detail = if detail.is_empty() {
        format!("exit_status={status}")
    } else {
        detail.to_string()
    };
    Err(SymphonyError::Workspace(format!(
        "git {operation} failed: {detail}"
    )))
}

fn redact_repository_url(message: &str, repository_url: &str) -> String {
    message.replace(repository_url, &redact_url_credentials(repository_url))
}

fn redact_url_credentials(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find('/')
        .map(|offset| authority_start + offset)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };

    format!(
        "{}***@{}{}",
        &url[..authority_start],
        &authority[at + 1..],
        &url[authority_end..]
    )
}
