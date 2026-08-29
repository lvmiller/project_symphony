use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::config::{
    DEFAULT_SOURCE_ID, HooksConfig, WorkspaceConfig, WorkspacePopulationConfig,
    normalize_absolute_path,
};
use crate::domain::{Issue, Workspace};
use crate::error::{Result, SymphonyError};
use crate::hooks::{
    HookKind, populate_workspace, run_hook_best_effort_with_source, run_hook_with_source,
};

#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    root: PathBuf,
    population: WorkspacePopulationConfig,
    hooks: HooksConfig,
}

impl WorkspaceManager {
    pub fn new(config: &WorkspaceConfig, hooks: HooksConfig) -> Result<Self> {
        let root = canonicalize_if_exists(&normalize_absolute_path(&config.root)?)?;
        Ok(Self {
            root,
            population: config.population.clone(),
            hooks,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_path_for_identifier(&self, identifier: &str) -> Result<(String, PathBuf)> {
        self.workspace_path_for_source_identifier(DEFAULT_SOURCE_ID, identifier)
    }

    pub fn workspace_path_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<(String, PathBuf)> {
        let key = source_workspace_key(source_id, identifier);
        let path = normalize_absolute_path(&self.root.join(&key))?;
        ensure_contained(&self.root, &path)?;
        Ok((key, path))
    }

    pub async fn create_for_issue(&self, issue: &Issue) -> Result<Workspace> {
        self.create_for_identifier(&issue.identifier).await
    }

    pub async fn create_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
    ) -> Result<Workspace> {
        self.create_for_source_identifier(source_id, &issue.identifier)
            .await
    }

    pub async fn create_for_identifier(&self, identifier: &str) -> Result<Workspace> {
        self.create_for_source_identifier(DEFAULT_SOURCE_ID, identifier)
            .await
    }

    pub async fn create_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<Workspace> {
        fs::create_dir_all(&self.root)
            .map_err(|err| SymphonyError::io(Some(self.root.clone()), err))?;
        let canonical_root = canonicalize_dir(&self.root)?;
        let source_key = sanitize_workspace_key(source_id);
        let workspace_key = source_workspace_key(source_id, identifier);
        let workspace_parent = if source_id == DEFAULT_SOURCE_ID {
            canonical_root.clone()
        } else {
            let source_path = normalize_absolute_path(&canonical_root.join(&source_key))?;
            ensure_contained(&canonical_root, &source_path)?;
            ensure_existing_directory(&canonical_root, &source_path, "workspace source path")?
        };
        let issue_key = sanitize_workspace_key(identifier);
        let path = normalize_absolute_path(&workspace_parent.join(&issue_key))?;
        ensure_contained(&canonical_root, &path)?;
        let (created_now, verified_path) = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(symlink_workspace_error(
                        "workspace path is a symlink",
                        &path,
                    ));
                }
                if !metadata.is_dir() {
                    return Err(SymphonyError::Workspace(format!(
                        "workspace path exists and is not a directory path={}",
                        path.display()
                    )));
                }
                (false, verify_workspace_dir(&canonical_root, &path)?)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path).map_err(|err| SymphonyError::io(Some(path.clone()), err))?;
                (true, verify_workspace_dir(&canonical_root, &path)?)
            }
            Err(err) => return Err(SymphonyError::io(Some(path.clone()), err)),
        };
        let workspace = Workspace {
            path: verified_path.clone(),
            workspace_key,
            created_now,
        };
        if let Err(error) = populate_workspace(&self.population, &verified_path, created_now).await
        {
            if created_now {
                remove_new_workspace(&canonical_root, &verified_path);
            }
            return Err(error);
        }
        if created_now
            && let Err(error) = run_hook_with_source(
                HookKind::AfterCreate,
                &self.hooks,
                &verified_path,
                Some(source_id),
            )
            .await
        {
            remove_new_workspace(&canonical_root, &verified_path);
            return Err(error);
        }
        Ok(workspace)
    }

    pub async fn before_run(&self, workspace: &Path) -> Result<()> {
        self.before_run_for_source(DEFAULT_SOURCE_ID, workspace)
            .await
    }

    pub async fn before_run_for_source(&self, source_id: &str, workspace: &Path) -> Result<()> {
        let canonical_root = canonicalize_dir(&self.root)?;
        let workspace = verify_workspace_dir(&canonical_root, workspace)?;
        run_hook_with_source(
            HookKind::BeforeRun,
            &self.hooks,
            &workspace,
            Some(source_id),
        )
        .await
    }

    pub async fn after_run_best_effort(&self, workspace: &Path) {
        self.after_run_best_effort_for_source(DEFAULT_SOURCE_ID, workspace)
            .await;
    }

    pub async fn after_run_best_effort_for_source(&self, source_id: &str, workspace: &Path) {
        let canonical_root = match canonicalize_dir(&self.root) {
            Ok(root) => root,
            Err(error) => {
                warn!(error = %error, "after_run hook skipped invalid workspace root");
                return;
            }
        };
        let workspace = match verify_workspace_dir(&canonical_root, workspace) {
            Ok(workspace) => workspace,
            Err(error) => {
                warn!(error = %error, "after_run hook skipped invalid workspace path");
                return;
            }
        };
        run_hook_best_effort_with_source(
            HookKind::AfterRun,
            &self.hooks,
            &workspace,
            Some(source_id),
        )
        .await;
    }

    pub async fn remove_for_issue(&self, issue: &Issue) -> Result<()> {
        self.remove_for_identifier(&issue.identifier).await
    }

    pub async fn remove_for_source_issue(&self, source_id: &str, issue: &Issue) -> Result<()> {
        self.remove_for_source_identifier(source_id, &issue.identifier)
            .await
    }

    pub async fn remove_for_identifier(&self, identifier: &str) -> Result<()> {
        self.remove_for_source_identifier(DEFAULT_SOURCE_ID, identifier)
            .await
    }

    pub async fn remove_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<()> {
        let canonical_root = canonicalize_dir(&self.root)?;
        let workspace_key = source_workspace_key(source_id, identifier);
        let path = normalize_absolute_path(&canonical_root.join(&workspace_key))?;
        ensure_contained(&canonical_root, &path)?;
        let workspace = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(symlink_workspace_error(
                        "workspace cleanup target is a symlink",
                        &path,
                    ));
                }
                if !metadata.is_dir() {
                    warn!(workspace = %path.display(), "workspace cleanup skipped non-directory path");
                    return Err(SymphonyError::Workspace(format!(
                        "workspace cleanup target is not a directory path={}",
                        path.display()
                    )));
                }
                verify_workspace_dir(&canonical_root, &path)?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(SymphonyError::io(Some(path.clone()), err)),
        };
        run_hook_best_effort_with_source(
            HookKind::BeforeRemove,
            &self.hooks,
            &workspace,
            Some(source_id),
        )
        .await;
        fs::remove_dir_all(&workspace)
            .map_err(|err| SymphonyError::io(Some(workspace.clone()), err))?;
        info!(workspace = %workspace.display(), "workspace removed");
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

pub fn source_workspace_key(source_id: &str, identifier: &str) -> String {
    let issue_key = sanitize_workspace_key(identifier);
    if source_id == DEFAULT_SOURCE_ID {
        issue_key
    } else {
        format!("{}/{}", sanitize_workspace_key(source_id), issue_key)
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

fn ensure_existing_directory(canonical_root: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(symlink_workspace_error(
                    &format!("{label} is a symlink"),
                    path,
                ));
            }
            if !metadata.is_dir() {
                return Err(SymphonyError::Workspace(format!(
                    "{label} exists and is not a directory path={}",
                    path.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|err| SymphonyError::io(Some(path.to_path_buf()), err))?;
        }
        Err(err) => return Err(SymphonyError::io(Some(path.to_path_buf()), err)),
    }
    verify_workspace_dir(canonical_root, path)
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).map_err(|err| SymphonyError::io(Some(path.to_path_buf()), err))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(SymphonyError::Workspace(format!(
            "workspace root is not a directory path={}",
            canonical.display()
        )))
    }
}

fn verify_workspace_dir(canonical_root: &Path, workspace: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(workspace)
        .map_err(|err| SymphonyError::io(Some(workspace.to_path_buf()), err))?;
    if metadata.file_type().is_symlink() {
        return Err(symlink_workspace_error(
            "workspace path is a symlink",
            workspace,
        ));
    }
    if !metadata.is_dir() {
        return Err(SymphonyError::Workspace(format!(
            "workspace path is not a directory path={}",
            workspace.display()
        )));
    }
    let canonical_workspace = fs::canonicalize(workspace)
        .map_err(|err| SymphonyError::io(Some(workspace.to_path_buf()), err))?;
    ensure_canonical_contained(canonical_root, &canonical_workspace)?;
    Ok(canonical_workspace)
}

fn ensure_canonical_contained(canonical_root: &Path, canonical_workspace: &Path) -> Result<()> {
    if canonical_workspace.starts_with(canonical_root) && canonical_workspace != canonical_root {
        Ok(())
    } else {
        Err(SymphonyError::Workspace(format!(
            "workspace path escapes root root={} path={}",
            canonical_root.display(),
            canonical_workspace.display()
        )))
    }
}

fn remove_new_workspace(canonical_root: &Path, workspace: &Path) {
    match verify_workspace_dir(canonical_root, workspace) {
        Ok(workspace) => {
            if let Err(error) = fs::remove_dir_all(&workspace) {
                warn!(workspace = %workspace.display(), error = %error, "failed to clean new partial workspace");
            }
        }
        Err(error) => {
            warn!(error = %error, "new partial workspace cleanup skipped invalid workspace path");
        }
    }
}

fn symlink_workspace_error(message: &str, path: &Path) -> SymphonyError {
    SymphonyError::Workspace(format!("{message} path={}", path.display()))
}
