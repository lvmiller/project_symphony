use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

use crate::config::HooksConfig;
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
