use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use crate::config::{
    DEFAULT_SOURCE_ID, HooksConfig, WorkspaceConfig, WorkspacePopulationConfig,
    normalize_absolute_path,
};
use crate::domain::{ExecutionTarget, Issue, Workspace};
use crate::error::{Result, SymphonyError};
use crate::hooks::{HookContext, HookKind, populate_workspace, run_hook, run_hook_best_effort};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    root: PathBuf,
    remote_root: String,
    population: WorkspacePopulationConfig,
    hooks: HooksConfig,
}

impl WorkspaceManager {
    pub fn new(config: &WorkspaceConfig, hooks: HooksConfig) -> Result<Self> {
        let root = canonicalize_if_exists(&normalize_absolute_path(&config.root)?)?;
        Ok(Self {
            root,
            remote_root: config.remote_root.clone(),
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

    fn remote_workspace_path_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<(String, PathBuf)> {
        let key = source_workspace_key(source_id, identifier);
        let root = self.remote_root.trim_end_matches('/');
        let path = PathBuf::from(format!("{root}/{key}"));
        if !self.remote_root.starts_with('/') || !path.to_string_lossy().starts_with('/') {
            return Err(SymphonyError::Workspace(format!(
                "remote workspace root must be an absolute POSIX path root={}",
                self.remote_root
            )));
        }
        Ok((key, path))
    }

    fn ensure_remote_contained(&self, workspace: &Path) -> Result<()> {
        let root = self.remote_root.trim_end_matches('/');
        let workspace = workspace.to_string_lossy();
        if self.remote_root.starts_with('/')
            && workspace.starts_with(root)
            && workspace.len() > root.len()
            && workspace.as_bytes().get(root.len()) == Some(&b'/')
        {
            Ok(())
        } else {
            Err(SymphonyError::Workspace(format!(
                "remote workspace path escapes root root={} path={workspace}",
                self.remote_root
            )))
        }
    }

    pub async fn create_for_target(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        issue: &Issue,
    ) -> Result<Workspace> {
        self.create_for_target_identifier(target, source_id, &issue.identifier)
            .await
    }

    async fn create_for_target_identifier(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        identifier: &str,
    ) -> Result<Workspace> {
        match target {
            ExecutionTarget::Local => {
                self.create_local_for_source_identifier(source_id, identifier)
                    .await
            }
            ExecutionTarget::Ssh { host } => {
                self.create_remote_for_source_identifier(host, source_id, identifier)
                    .await
            }
        }
    }

    pub async fn create_for_issue(&self, issue: &Issue) -> Result<Workspace> {
        self.create_for_target(&ExecutionTarget::Local, DEFAULT_SOURCE_ID, issue)
            .await
    }

    pub async fn create_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
    ) -> Result<Workspace> {
        self.create_for_target(&ExecutionTarget::Local, source_id, issue)
            .await
    }

    pub async fn create_for_identifier(&self, identifier: &str) -> Result<Workspace> {
        self.create_for_target_identifier(&ExecutionTarget::Local, DEFAULT_SOURCE_ID, identifier)
            .await
    }

    pub async fn create_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<Workspace> {
        self.create_for_target_identifier(&ExecutionTarget::Local, source_id, identifier)
            .await
    }

    async fn create_local_for_source_identifier(
        &self,
        source_id: &str,
        identifier: &str,
    ) -> Result<Workspace> {
        fs::create_dir_all(&self.root)
            .map_err(|err| SymphonyError::io(Some(self.root.clone()), err))?;
        let canonical_root = canonicalize_dir(&self.root)?;
        let source_key = source_workspace_namespace(source_id);
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
            && let Err(error) = run_hook(
                HookKind::AfterCreate,
                &self.hooks,
                HookContext::new(
                    &verified_path,
                    workspace.workspace_key.clone(),
                    source_id,
                    identifier,
                ),
            )
            .await
        {
            remove_new_workspace(&canonical_root, &verified_path);
            return Err(error);
        }
        Ok(workspace)
    }

    pub async fn before_run_for_target(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) -> Result<()> {
        match target {
            ExecutionTarget::Local => {
                self.before_run_local_for_source_issue(source_id, issue, workspace)
                    .await
            }
            ExecutionTarget::Ssh { host } => {
                self.run_remote_hook(
                    host,
                    HookKind::BeforeRun,
                    source_id,
                    &issue.identifier,
                    workspace,
                    false,
                )
                .await
            }
        }
    }

    pub async fn before_run_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) -> Result<()> {
        self.before_run_for_target(&ExecutionTarget::Local, source_id, issue, workspace)
            .await
    }

    async fn before_run_local_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) -> Result<()> {
        let canonical_root = canonicalize_dir(&self.root)?;
        let workspace = verify_workspace_dir(&canonical_root, workspace)?;
        run_hook(
            HookKind::BeforeRun,
            &self.hooks,
            HookContext::new(
                &workspace,
                source_workspace_key(source_id, &issue.identifier),
                source_id,
                &issue.identifier,
            ),
        )
        .await
    }

    pub async fn after_run_best_effort_for_target(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) {
        match target {
            ExecutionTarget::Local => {
                self.after_run_local_best_effort_for_source_issue(source_id, issue, workspace)
                    .await
            }
            ExecutionTarget::Ssh { host } => {
                if let Err(error) = self
                    .run_remote_hook(
                        host,
                        HookKind::AfterRun,
                        source_id,
                        &issue.identifier,
                        workspace,
                        true,
                    )
                    .await
                {
                    warn!(host, error = %error, "remote after_run hook failed ignored");
                }
            }
        }
    }

    pub async fn after_run_best_effort_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) {
        self.after_run_best_effort_for_target(&ExecutionTarget::Local, source_id, issue, workspace)
            .await;
    }

    async fn after_run_local_best_effort_for_source_issue(
        &self,
        source_id: &str,
        issue: &Issue,
        workspace: &Path,
    ) {
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
        run_hook_best_effort(
            HookKind::AfterRun,
            &self.hooks,
            HookContext::new(
                &workspace,
                source_workspace_key(source_id, &issue.identifier),
                source_id,
                &issue.identifier,
            ),
        )
        .await;
    }

    pub async fn remove_for_target(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        issue: &Issue,
    ) -> Result<()> {
        match target {
            ExecutionTarget::Local => {
                self.remove_local_for_source_identifier(source_id, &issue.identifier)
                    .await
            }
            ExecutionTarget::Ssh { host } => {
                self.remove_remote_for_source_identifier(host, source_id, &issue.identifier)
                    .await
            }
        }
    }

    pub async fn remove_for_issue(&self, issue: &Issue) -> Result<()> {
        self.remove_for_target(&ExecutionTarget::Local, DEFAULT_SOURCE_ID, issue)
            .await
    }

    pub async fn remove_for_source_issue(&self, source_id: &str, issue: &Issue) -> Result<()> {
        self.remove_for_target(&ExecutionTarget::Local, source_id, issue)
            .await
    }

    pub async fn remove_for_identifier(&self, identifier: &str) -> Result<()> {
        self.remove_for_target_identifier(&ExecutionTarget::Local, DEFAULT_SOURCE_ID, identifier)
            .await
    }

    async fn remove_for_target_identifier(
        &self,
        target: &ExecutionTarget,
        source_id: &str,
        identifier: &str,
    ) -> Result<()> {
        match target {
            ExecutionTarget::Local => {
                self.remove_local_for_source_identifier(source_id, identifier)
                    .await
            }
            ExecutionTarget::Ssh { host } => {
                self.remove_remote_for_source_identifier(host, source_id, identifier)
                    .await
            }
        }
    }

    async fn remove_local_for_source_identifier(
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
        run_hook_best_effort(
            HookKind::BeforeRemove,
            &self.hooks,
            HookContext::new(&workspace, workspace_key, source_id, identifier),
        )
        .await;
        fs::remove_dir_all(&workspace)
            .map_err(|err| SymphonyError::io(Some(workspace.clone()), err))?;
        info!(workspace = %workspace.display(), "workspace removed");
        Ok(())
    }

    async fn create_remote_for_source_identifier(
        &self,
        host: &str,
        source_id: &str,
        identifier: &str,
    ) -> Result<Workspace> {
        let (workspace_key, path) =
            self.remote_workspace_path_for_source_identifier(source_id, identifier)?;
        let parent = path.parent().ok_or_else(|| {
            SymphonyError::Workspace(format!(
                "remote workspace has no parent path={}",
                path.display()
            ))
        })?;
        let output = self
            .run_ssh(
                host,
                &remote_create_script(Path::new(&self.remote_root), parent, &path),
            )
            .await?;
        let created_now = match String::from_utf8_lossy(&output.stdout).trim() {
            "created" => true,
            "reused" => false,
            result => {
                return Err(SymphonyError::Workspace(format!(
                    "remote workspace create returned an invalid result host={host} result={result:?}"
                )));
            }
        };
        let workspace = Workspace {
            path: path.clone(),
            workspace_key,
            created_now,
        };
        if let Err(error) = self
            .populate_remote_workspace(host, &path, created_now)
            .await
        {
            if created_now {
                let _ = self
                    .remove_remote_for_source_identifier(host, source_id, identifier)
                    .await;
            }
            return Err(error);
        }
        if created_now
            && let Err(error) = self
                .run_remote_hook(
                    host,
                    HookKind::AfterCreate,
                    source_id,
                    identifier,
                    &path,
                    false,
                )
                .await
        {
            let _ = self
                .remove_remote_for_source_identifier(host, source_id, identifier)
                .await;
            return Err(error);
        }
        Ok(workspace)
    }

    async fn remove_remote_for_source_identifier(
        &self,
        host: &str,
        source_id: &str,
        identifier: &str,
    ) -> Result<()> {
        let (_, path) = self.remote_workspace_path_for_source_identifier(source_id, identifier)?;
        if let Err(error) = self
            .run_remote_hook(
                host,
                HookKind::BeforeRemove,
                source_id,
                identifier,
                &path,
                true,
            )
            .await
        {
            warn!(host, error = %error, "remote before_remove hook failed ignored");
        }
        self.run_ssh(
            host,
            &remote_remove_script(Path::new(&self.remote_root), &path),
        )
        .await?;
        info!(host, workspace = %path.display(), "remote workspace removed");
        Ok(())
    }

    async fn populate_remote_workspace(
        &self,
        host: &str,
        workspace: &Path,
        created_now: bool,
    ) -> Result<()> {
        self.ensure_remote_contained(workspace)?;
        let script = remote_population_script(&self.population, workspace, created_now)?;
        if let Some(script) = script {
            self.run_ssh(host, &script).await?;
        }
        Ok(())
    }

    async fn run_remote_hook(
        &self,
        host: &str,
        kind: HookKind,
        source_id: &str,
        identifier: &str,
        workspace: &Path,
        ignore_missing_workspace: bool,
    ) -> Result<()> {
        self.ensure_remote_contained(workspace)?;
        let Some(script) = remote_hook_script(
            kind,
            &self.hooks,
            source_id,
            identifier,
            workspace,
            ignore_missing_workspace,
        ) else {
            return Ok(());
        };
        self.run_ssh(host, &script).await.map(|_| ())
    }

    async fn run_ssh(&self, host: &str, script: &str) -> Result<std::process::Output> {
        let output = Command::new("ssh")
            .args(ssh_command_arguments(host, script))
            .output()
            .await
            .map_err(|error| {
                SymphonyError::Workspace(format!(
                    "remote SSH command failed to start host={host}: {error}"
                ))
            })?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(SymphonyError::Workspace(format!(
                "remote SSH command failed host={host} exit_status={}",
                output.status
            )))
        }
    }

    /// Removes stale, unclaimed workspace directories in one source namespace.
    ///
    /// The caller decides when to invoke this; Symphony uses it only during startup.
    pub async fn prune_orphaned_workspaces_for_source(
        &self,
        source_id: &str,
        protected_workspace_keys: &BTreeSet<String>,
        max_age_days: u64,
    ) -> Result<()> {
        self.prune_orphaned_workspaces_for_source_with_namespaces(
            source_id,
            protected_workspace_keys,
            &BTreeSet::new(),
            max_age_days,
        )
        .await
    }

    /// Removes stale, unclaimed workspace directories, excluding other configured source namespaces.
    pub async fn prune_orphaned_workspaces_for_source_with_namespaces(
        &self,
        source_id: &str,
        protected_workspace_keys: &BTreeSet<String>,
        source_namespace_segments: &BTreeSet<String>,
        max_age_days: u64,
    ) -> Result<()> {
        self.prune_orphaned_workspaces_for_source_at(
            source_id,
            protected_workspace_keys,
            source_namespace_segments,
            max_age_days,
            SystemTime::now(),
        )
        .await
    }
    async fn prune_orphaned_workspaces_for_source_at(
        &self,
        source_id: &str,
        protected_workspace_keys: &BTreeSet<String>,
        source_namespace_segments: &BTreeSet<String>,
        max_age_days: u64,
        now: SystemTime,
    ) -> Result<()> {
        let canonical_root = match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                warn!(workspace_root = %self.root.display(), "workspace retention skipped symlink root");
                return Ok(());
            }
            Ok(metadata) if !metadata.is_dir() => {
                warn!(workspace_root = %self.root.display(), "workspace retention skipped non-directory root");
                return Ok(());
            }
            Ok(_) => canonicalize_dir(&self.root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(SymphonyError::io(Some(self.root.clone()), error)),
        };
        let namespace = if source_id == DEFAULT_SOURCE_ID {
            canonical_root.clone()
        } else {
            let source_key = source_workspace_namespace(source_id);
            let path = normalize_absolute_path(&canonical_root.join(&source_key))?;
            ensure_contained(&canonical_root, &path)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    warn!(workspace_namespace = %path.display(), "workspace retention skipped symlink namespace");
                    return Ok(());
                }
                Ok(metadata) if !metadata.is_dir() => {
                    warn!(workspace_namespace = %path.display(), "workspace retention skipped non-directory namespace");
                    return Ok(());
                }
                Ok(_) => match verify_workspace_dir(&canonical_root, &path) {
                    Ok(path) => path,
                    Err(error) => {
                        warn!(workspace_namespace = %path.display(), error = %error, "workspace retention skipped invalid namespace");
                        return Ok(());
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(SymphonyError::io(Some(path), error)),
            }
        };
        let max_age = Duration::from_secs(max_age_days.saturating_mul(24 * 60 * 60));
        let entries = fs::read_dir(&namespace)
            .map_err(|error| SymphonyError::io(Some(namespace.clone()), error))?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(workspace_namespace = %namespace.display(), error = %error, "workspace retention skipped unreadable entry");
                    continue;
                }
            };
            let entry_name = match entry.file_name().into_string() {
                Ok(entry_name) if is_workspace_segment(&entry_name) => entry_name,
                Ok(entry_name) => {
                    warn!(workspace_namespace = %namespace.display(), entry = %entry_name, "workspace retention skipped malformed entry");
                    continue;
                }
                Err(_) => {
                    warn!(workspace_namespace = %namespace.display(), "workspace retention skipped non-utf8 entry");
                    continue;
                }
            };
            let workspace_key = source_workspace_key(source_id, &entry_name);
            if source_id == DEFAULT_SOURCE_ID && source_namespace_segments.contains(&entry_name) {
                continue;
            }
            if protected_workspace_keys.contains(&workspace_key) {
                continue;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    warn!(workspace = %path.display(), "workspace retention skipped symlink entry");
                    continue;
                }
                Ok(metadata) if !metadata.is_dir() => {
                    warn!(workspace = %path.display(), "workspace retention skipped non-directory entry");
                    continue;
                }
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!(workspace = %path.display(), error = %error, "workspace retention skipped unreadable entry");
                    continue;
                }
            };
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    warn!(workspace = %path.display(), error = %error, "workspace retention skipped entry without modification time");
                    continue;
                }
            };
            if !is_older_than(modified, now, max_age) {
                continue;
            }
            let workspace = match verify_workspace_dir(&canonical_root, &path) {
                Ok(workspace) => workspace,
                Err(error) => {
                    warn!(workspace = %path.display(), error = %error, "workspace retention skipped invalid entry");
                    continue;
                }
            };
            run_hook_best_effort(
                HookKind::BeforeRemove,
                &self.hooks,
                HookContext::new(&workspace, workspace_key, source_id, &entry_name),
            )
            .await;
            match fs::remove_dir_all(&workspace) {
                Ok(()) => info!(workspace = %workspace.display(), "stale workspace removed"),
                Err(error) => {
                    warn!(workspace = %workspace.display(), error = %error, "workspace retention removal failed")
                }
            }
        }
        Ok(())
    }
}

fn ssh_command_arguments(host: &str, script: &str) -> [String; 4] {
    [
        host.to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        script.to_string(),
    ]
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn remote_create_script(root: &Path, parent: &Path, workspace: &Path) -> String {
    let root = shell_quote(&root.to_string_lossy());
    let parent = shell_quote(&parent.to_string_lossy());
    let workspace = shell_quote(&workspace.to_string_lossy());
    format!(
        "set -eu; root={root}; parent={parent}; workspace={workspace}; \
         case \"$workspace\" in \"$root\"/*) ;; *) exit 64 ;; esac; \
         [ ! -L \"$root\" ] || exit 65; mkdir -p \"$root\"; \
         [ ! -L \"$parent\" ] || exit 66; mkdir -p \"$parent\"; \
         if [ -e \"$workspace\" ] || [ -L \"$workspace\" ]; then \
           [ ! -L \"$workspace\" ] && [ -d \"$workspace\" ] || exit 67; printf reused; \
         else mkdir \"$workspace\"; printf created; fi"
    )
}

fn remote_remove_script(root: &Path, workspace: &Path) -> String {
    let root = shell_quote(&root.to_string_lossy());
    let workspace = shell_quote(&workspace.to_string_lossy());
    format!(
        "set -eu; root={root}; workspace={workspace}; \
         case \"$workspace\" in \"$root\"/*) ;; *) exit 64 ;; esac; \
         if [ ! -e \"$workspace\" ] && [ ! -L \"$workspace\" ]; then exit 0; fi; \
         [ ! -L \"$workspace\" ] && [ -d \"$workspace\" ] || exit 67; \
         rm -rf \"$workspace\""
    )
}

fn remote_population_script(
    population: &WorkspacePopulationConfig,
    workspace: &Path,
    created_now: bool,
) -> Result<Option<String>> {
    let workspace = shell_quote(&workspace.to_string_lossy());
    let Some(repository_url) = population.repository_url.as_deref() else {
        return match population.kind {
            crate::config::WorkspacePopulationKind::None => Ok(None),
            crate::config::WorkspacePopulationKind::Git => Err(SymphonyError::Workspace(
                "git workspace population is missing repository_url".to_string(),
            )),
        };
    };
    match population.kind {
        crate::config::WorkspacePopulationKind::None => Ok(None),
        crate::config::WorkspacePopulationKind::Git if created_now => {
            let mut command = String::from("git clone");
            if let Some(depth) = population.depth {
                command.push_str(&format!(" --depth {depth}"));
            }
            if let Some(branch) = &population.branch {
                command.push_str(" --branch ");
                command.push_str(&shell_quote(branch));
            }
            command.push_str(" -- ");
            command.push_str(&shell_quote(repository_url));
            command.push_str(" .");
            if let Some(reference) = &population.reference {
                command.push_str(" && git checkout --detach ");
                command.push_str(&shell_quote(reference));
            }
            Ok(Some(format!("set -eu; cd {workspace}; {command}")))
        }
        crate::config::WorkspacePopulationKind::Git
            if matches!(
                population.reuse,
                crate::config::WorkspacePopulationReusePolicy::FetchFfOnly
            ) =>
        {
            let target = population
                .branch
                .as_deref()
                .or(population.reference.as_deref())
                .unwrap_or("");
            let fetch_target = if target.is_empty() {
                String::new()
            } else {
                format!(" {}", shell_quote(target))
            };
            let merge_target = if target.is_empty() {
                "@{upstream}"
            } else {
                "FETCH_HEAD"
            };
            Ok(Some(format!(
                "set -eu; cd {workspace}; git fetch --no-tags origin{fetch_target}; git merge --ff-only {merge_target}"
            )))
        }
        crate::config::WorkspacePopulationKind::Git => Ok(None),
    }
}

fn remote_hook_script(
    kind: HookKind,
    hooks: &HooksConfig,
    source_id: &str,
    identifier: &str,
    workspace: &Path,
    ignore_missing_workspace: bool,
) -> Option<String> {
    let hook = match kind {
        HookKind::AfterCreate => hooks.after_create.as_deref(),
        HookKind::BeforeRun => hooks.before_run.as_deref(),
        HookKind::AfterRun => hooks.after_run.as_deref(),
        HookKind::BeforeRemove => hooks.before_remove.as_deref(),
    }?;
    let workspace = shell_quote(&workspace.to_string_lossy());
    let workspace_key = shell_quote(&source_workspace_key(source_id, identifier));
    let source_key = shell_quote(&source_workspace_namespace(source_id));
    let issue_key = shell_quote(&sanitize_workspace_key(identifier));
    let source_id = shell_quote(source_id);
    let identifier = shell_quote(identifier);
    let missing = if ignore_missing_workspace {
        "if [ ! -d \"$workspace\" ] || [ -L \"$workspace\" ]; then exit 0; fi; "
    } else {
        "[ -d \"$workspace\" ] && [ ! -L \"$workspace\" ] || exit 67; "
    };
    Some(format!(
        "set -eu; workspace={workspace}; {missing} cd \"$workspace\"; \
         export SYMPHONY_HOOK_NAME={} SYMPHONY_WORKSPACE_PATH=\"$workspace\" \
         SYMPHONY_WORKSPACE_KEY={workspace_key} SYMPHONY_SOURCE_ID={source_id} \
         SYMPHONY_SOURCE_KEY={source_key} SYMPHONY_ISSUE_IDENTIFIER={identifier} \
         SYMPHONY_ISSUE_KEY={issue_key}; sh -lc {}",
        shell_quote(kind.name()),
        shell_quote(hook),
    ))
}

/// Returns an injective, lowercase-ASCII namespace segment for a source ID.
///
/// Encoding UTF-8 bytes avoids aliases caused by filesystem case folding or Unicode
/// normalization while remaining deterministic on every supported platform.
pub fn source_workspace_namespace(source_id: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut namespace = String::with_capacity("source-".len() + source_id.len() * 2);
    namespace.push_str("source-");
    for &byte in source_id.as_bytes() {
        namespace.push(HEX[usize::from(byte >> 4)] as char);
        namespace.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    namespace
}

fn is_workspace_segment(segment: &str) -> bool {
    !segment.is_empty() && sanitize_workspace_key(segment) == segment
}

fn is_older_than(modified: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    now.duration_since(modified)
        .is_ok_and(|elapsed| elapsed >= max_age)
}

/// Produces a single safe directory segment for an issue identifier.
///
/// This remains deliberately separate from [`source_workspace_namespace`]: issue
/// identifiers retain the established readable key format, while source namespaces
/// must be injective across case and Unicode filesystem aliases.
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
        format!("{}/{}", source_workspace_namespace(source_id), issue_key)
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

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn manager(root: &Path, hooks: HooksConfig) -> WorkspaceManager {
        WorkspaceManager::new(
            &WorkspaceConfig {
                root: root.to_path_buf(),
                remote_root: root.to_string_lossy().replace('\\', "/"),
                cleanup: Default::default(),
                retention: Default::default(),
                population: Default::default(),
            },
            hooks,
        )
        .expect("workspace manager")
    }

    #[test]
    fn remote_workspace_commands_use_posix_paths_and_fixed_ssh_argv() {
        let root = Path::new("/srv/symphony");
        let workspace = Path::new("/srv/symphony/source-5465616d/issue_123");
        let script = remote_create_script(root, workspace.parent().unwrap(), workspace);
        let arguments = ssh_command_arguments("worker-a", &script);

        assert_eq!(arguments[0], "worker-a");
        assert_eq!(arguments[1], "sh");
        assert_eq!(arguments[2], "-lc");
        assert!(arguments[3].contains("workspace='/srv/symphony/source-5465616d/issue_123'"));
        assert!(arguments[3].contains("case \"$workspace\" in \"$root\"/*)"));
        assert!(arguments[3].contains("[ ! -L \"$workspace\" ]"));
    }

    #[test]
    fn remote_workspace_paths_are_posix_and_contained() {
        let temp = TempDir::new().expect("tempdir");
        let manager = WorkspaceManager::new(
            &WorkspaceConfig {
                root: temp.path().to_path_buf(),
                remote_root: "/srv/symphony".to_string(),
                cleanup: Default::default(),
                retention: Default::default(),
                population: Default::default(),
            },
            HooksConfig::default(),
        )
        .expect("workspace manager");
        let (key, path) = manager
            .remote_workspace_path_for_source_identifier("Team", "issue/123")
            .expect("remote path");

        assert_eq!(key, "source-5465616d/issue_123");
        assert_eq!(
            path.to_string_lossy(),
            "/srv/symphony/source-5465616d/issue_123"
        );
        manager
            .ensure_remote_contained(&path)
            .expect("contained remote path");
        assert!(
            manager
                .ensure_remote_contained(Path::new("/tmp/escape"))
                .is_err()
        );
    }
    #[tokio::test]
    async fn retention_removes_only_old_unprotected_valid_orphans() {
        let temp = TempDir::new().expect("tempdir");
        let stale = temp.path().join("stale");
        let protected = temp.path().join("protected");
        let malformed = temp.path().join("bad name");
        let regular_file = temp.path().join("not-a-directory");
        fs::create_dir_all(&stale).expect("stale workspace");
        fs::create_dir_all(&protected).expect("protected workspace");
        fs::create_dir_all(&malformed).expect("malformed entry");
        fs::write(&regular_file, "not a workspace").expect("regular entry");
        let old = fs::metadata(&stale)
            .expect("stale metadata")
            .modified()
            .expect("stale modification time")
            + Duration::from_secs(2 * 24 * 60 * 60);
        let mut protected_keys = BTreeSet::new();
        protected_keys.insert("protected".to_string());

        manager(temp.path(), HooksConfig::default())
            .prune_orphaned_workspaces_for_source_at(
                DEFAULT_SOURCE_ID,
                &protected_keys,
                &BTreeSet::new(),
                1,
                old,
            )
            .await
            .expect("retention prune");

        assert!(!stale.exists());
        assert!(protected.exists());
        assert!(malformed.exists());
        assert!(regular_file.exists());
    }

    #[test]
    fn retention_preserves_young_or_future_entries() {
        let now = SystemTime::now();
        assert!(!is_older_than(now, now, Duration::from_secs(24 * 60 * 60)));
        assert!(!is_older_than(
            now + Duration::from_secs(1),
            now,
            Duration::from_secs(24 * 60 * 60),
        ));
    }

    #[tokio::test]
    async fn default_namespace_does_not_prune_configured_source_namespaces() {
        let temp = TempDir::new().expect("tempdir");
        let source_namespace = temp.path().join("api");
        fs::create_dir_all(source_namespace.join("orphan")).expect("source workspace");
        let modified = fs::metadata(&source_namespace)
            .expect("namespace metadata")
            .modified()
            .expect("namespace modification time");
        let mut source_namespaces = BTreeSet::new();
        source_namespaces.insert("api".to_string());

        manager(temp.path(), HooksConfig::default())
            .prune_orphaned_workspaces_for_source_at(
                DEFAULT_SOURCE_ID,
                &BTreeSet::new(),
                &source_namespaces,
                1,
                modified + Duration::from_secs(2 * 24 * 60 * 60),
            )
            .await
            .expect("retention prune");

        assert!(source_namespace.exists());
    }
    #[tokio::test]
    async fn retention_skips_missing_root_without_creating_or_scanning_it() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("missing");
        manager(&root, HooksConfig::default())
            .prune_orphaned_workspaces_for_source(DEFAULT_SOURCE_ID, &BTreeSet::new(), 1)
            .await
            .expect("missing root is not an error");
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retention_skips_symlink_entries_without_touching_their_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        let link = temp.path().join("escaped");
        symlink(outside.path(), &link).expect("workspace symlink");
        let now = SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60);

        manager(temp.path(), HooksConfig::default())
            .prune_orphaned_workspaces_for_source_at(
                DEFAULT_SOURCE_ID,
                &BTreeSet::new(),
                &BTreeSet::new(),
                1,
                now,
            )
            .await
            .expect("retention prune");

        assert!(outside.path().exists());
        assert!(
            fs::symlink_metadata(&link)
                .expect("symlink remains")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retention_runs_before_remove_hook_best_effort() {
        let temp = TempDir::new().expect("tempdir");
        let stale = temp.path().join("stale");
        fs::create_dir_all(&stale).expect("stale workspace");
        let modified = fs::metadata(&stale)
            .expect("stale metadata")
            .modified()
            .expect("stale modification time");
        let hooks = HooksConfig {
            before_remove: Some("printf ran > ../retention_hook_marker; exit 7".to_string()),
            ..HooksConfig::default()
        };

        manager(temp.path(), hooks)
            .prune_orphaned_workspaces_for_source_at(
                DEFAULT_SOURCE_ID,
                &BTreeSet::new(),
                &BTreeSet::new(),
                1,
                modified + Duration::from_secs(2 * 24 * 60 * 60),
            )
            .await
            .expect("hook failure is best effort");

        assert!(!stale.exists());
        assert_eq!(
            fs::read_to_string(temp.path().join("retention_hook_marker")).expect("hook marker"),
            "ran"
        );
    }
}
