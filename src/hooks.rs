use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

use crate::config::{
    HooksConfig, WorkspacePopulationConfig, WorkspacePopulationKind, WorkspacePopulationReusePolicy,
};
use crate::error::{Result, SymphonyError};

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

pub async fn run_hook(kind: HookKind, hooks: &HooksConfig, workspace: &Path) -> Result<()> {
    run_hook_with_source(kind, hooks, workspace, None).await
}

pub async fn run_hook_with_source(
    kind: HookKind,
    hooks: &HooksConfig,
    workspace: &Path,
    source_id: Option<&str>,
) -> Result<()> {
    let Some(script) = kind.script(hooks) else {
        return Ok(());
    };
    info!(hook = kind.name(), workspace = %workspace.display(), "hook started");
    let mut child = Command::new("sh")
        .arg("-lc")
        .arg(script)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .envs(
            source_id
                .into_iter()
                .map(|source_id| ("SYMPHONY_SOURCE_ID", source_id)),
        )
        .spawn()
        .map_err(|err| SymphonyError::Hook {
            hook: kind.name(),
            message: format!("spawn failed: {err}"),
        })?;

    let timeout_ms = hooks.timeout_ms;
    let status = match timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(result) => result.map_err(|err| SymphonyError::Hook {
            hook: kind.name(),
            message: format!("wait failed: {err}"),
        })?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(SymphonyError::Hook {
                hook: kind.name(),
                message: format!("timeout after {timeout_ms} ms"),
            });
        }
    };

    if status.success() {
        info!(hook = kind.name(), workspace = %workspace.display(), "hook completed");
        Ok(())
    } else {
        Err(SymphonyError::Hook {
            hook: kind.name(),
            message: format!("exit_status={status}"),
        })
    }
}

pub async fn run_hook_best_effort(kind: HookKind, hooks: &HooksConfig, workspace: &Path) {
    run_hook_best_effort_with_source(kind, hooks, workspace, None).await;
}

pub async fn run_hook_best_effort_with_source(
    kind: HookKind,
    hooks: &HooksConfig,
    workspace: &Path,
    source_id: Option<&str>,
) {
    if let Err(error) = run_hook_with_source(kind, hooks, workspace, source_id).await {
        warn!(hook = kind.name(), error = %error, "hook failed ignored");
    }
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
