use std::fs;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::config::{HooksConfig, WorkspaceConfig, normalize_absolute_path};
use crate::domain::{Issue, Workspace};
use crate::error::{Result, SymphonyError};
use crate::hooks::{HookKind, run_hook, run_hook_best_effort};

#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    root: PathBuf,
    hooks: HooksConfig,
}

impl WorkspaceManager {
    pub fn new(config: &WorkspaceConfig, hooks: HooksConfig) -> Result<Self> {
        let root = canonicalize_if_exists(&normalize_absolute_path(&config.root)?)?;
        Ok(Self { root, hooks })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_path_for_identifier(&self, identifier: &str) -> Result<(String, PathBuf)> {
        let key = sanitize_workspace_key(identifier);
        let path = normalize_absolute_path(&self.root.join(&key))?;
        ensure_contained(&self.root, &path)?;
        Ok((key, path))
    }

    pub async fn create_for_issue(&self, issue: &Issue) -> Result<Workspace> {
        self.create_for_identifier(&issue.identifier).await
    }

    pub async fn create_for_identifier(&self, identifier: &str) -> Result<Workspace> {
        fs::create_dir_all(&self.root)
            .map_err(|err| SymphonyError::io(Some(self.root.clone()), err))?;
        let (workspace_key, path) = self.workspace_path_for_identifier(identifier)?;
        let created_now = if path.exists() {
            if !path.is_dir() {
                return Err(SymphonyError::Workspace(format!(
                    "workspace path exists and is not a directory path={}",
                    path.display()
                )));
            }
            false
        } else {
            fs::create_dir(&path).map_err(|err| SymphonyError::io(Some(path.clone()), err))?;
            true
        };
        let workspace = Workspace {
            path: path.clone(),
            workspace_key,
            created_now,
        };
        if created_now && let Err(error) = run_hook(HookKind::AfterCreate, &self.hooks, &path).await
        {
            let _ = fs::remove_dir_all(&path);
            return Err(error);
        }
        Ok(workspace)
    }

    pub async fn before_run(&self, workspace: &Path) -> Result<()> {
        let workspace = normalize_absolute_path(workspace)?;
        ensure_contained(&self.root, &workspace)?;
        run_hook(HookKind::BeforeRun, &self.hooks, &workspace).await
    }

    pub async fn after_run_best_effort(&self, workspace: &Path) {
        let workspace = match normalize_absolute_path(workspace) {
            Ok(workspace) => workspace,
            Err(error) => {
                warn!(error = %error, "after_run hook skipped invalid workspace path");
                return;
            }
        };
        if let Err(error) = ensure_contained(&self.root, &workspace) {
            warn!(error = %error, "after_run hook skipped escaped workspace path");
            return;
        }
        run_hook_best_effort(HookKind::AfterRun, &self.hooks, &workspace).await;
    }

    pub async fn remove_for_issue(&self, issue: &Issue) -> Result<()> {
        self.remove_for_identifier(&issue.identifier).await
    }

    pub async fn remove_for_identifier(&self, identifier: &str) -> Result<()> {
        let (_, path) = self.workspace_path_for_identifier(identifier)?;
        if !path.exists() {
            return Ok(());
        }
        if !path.is_dir() {
            warn!(workspace = %path.display(), "workspace cleanup skipped non-directory path");
            return Err(SymphonyError::Workspace(format!(
                "workspace cleanup target is not a directory path={}",
                path.display()
            )));
        }
        run_hook_best_effort(HookKind::BeforeRemove, &self.hooks, &path).await;
        fs::remove_dir_all(&path).map_err(|err| SymphonyError::io(Some(path.clone()), err))?;
        info!(workspace = %path.display(), "workspace removed");
        Ok(())
    }
}

pub fn sanitize_workspace_key(identifier: &str) -> String {
    let key: String = identifier
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if key.is_empty() || key == "." || key == ".." {
        "_".to_string()
    } else {
        key
    }
}

pub fn ensure_contained(root: &Path, workspace_path: &Path) -> Result<()> {
    let root = normalize_absolute_path(root)?;
    let workspace_path = normalize_absolute_path(workspace_path)?;
    if workspace_path.starts_with(&root) && workspace_path != root {
        Ok(())
    } else {
        Err(SymphonyError::Workspace(format!(
            "workspace path escapes root root={} path={}",
            root.display(),
            workspace_path.display()
        )))
    }
}

fn canonicalize_if_exists(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        fs::canonicalize(path).map_err(|err| SymphonyError::io(Some(path.to_path_buf()), err))
    } else {
        Ok(path.to_path_buf())
    }
}
