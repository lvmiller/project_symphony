use base64::{
    Engine as _,
    engine::general_purpose::{
        STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD,
    },
};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::{DirectCommitCompletionConfig, EffectiveConfig};
use crate::domain::Issue;
use crate::error::{Result, SymphonyError};
use crate::tracker::TrackerWriter;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionPlan {
    pub severity: String,
    pub target_state: String,
    pub detected_changes: Vec<String>,
    pub commit_title: String,
    pub commit_body: String,
    pub rebase_required: bool,
    pub planned_mutations: Vec<CompletionMutation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionMutation {
    StageAllChanges,
    Commit { title: String, body: String },
    FetchBaseBranch { base_branch: String },
    RebaseOntoBaseBranch { base_branch: String },
    PushBaseBranch { base_branch: String },
    MoveIssueToState { target_state: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionPartialFailure {
    pub pushed_commit_sha: String,
    pub target_state: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResult {
    pub commit_sha: Option<String>,
    pub moved_to_state: Option<String>,
    pub severity: Option<String>,
    pub skipped_reason: Option<String>,
    pub plan: Option<CompletionPlan>,
    pub partial_failure: Option<CompletionPartialFailure>,
}

impl CompletionResult {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            commit_sha: None,
            moved_to_state: None,
            severity: None,
            skipped_reason: Some(reason.into()),
            plan: None,
            partial_failure: None,
        }
    }

    pub fn is_committed_success(&self) -> bool {
        self.commit_sha.is_some() && self.moved_to_state.is_some() && self.partial_failure.is_none()
    }
}

struct DryRunPlanInput {
    severity: Severity,
    target_state: String,
    detected_changes: Vec<String>,
    has_workspace_changes: bool,
    has_pending_local_commits: bool,
}

pub struct DirectCommitCompletion {
    direct_commit: DirectCommitCompletionConfig,
    token: String,
    writer: Arc<dyn TrackerWriter>,
    test_authenticated_remote_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedGithubRepository {
    owner: String,
    name: String,
    issue_id: Option<String>,
    issue_identifier: Option<String>,
    issue_url: Option<String>,
}

impl ExpectedGithubRepository {
    pub fn from_configured_issue(config: &EffectiveConfig, issue: &Issue) -> Result<Self> {
        let github = config.tracker.github.as_ref().ok_or_else(|| {
            completion_error(
                "completion_repository_unavailable",
                "direct-commit completion requires GitHub repository configuration",
            )
        })?;
        let mut repository = Self::from_issue_identifier(&issue.identifier)?;
        if !github.issue_matches_configured_repository(&repository.owner, &repository.name) {
            return Err(completion_error(
                "completion_repository_untrusted",
                format!(
                    "issue repository {}/{} is not configured for this source",
                    repository.owner, repository.name
                ),
            ));
        }
        if let Some(url) = issue.url.as_deref() {
            let url_repository = Self::from_issue_url(url)?;
            if !repository.same_identity(&url_repository) {
                return Err(completion_error(
                    "completion_repository_mismatch",
                    "issue URL and identifier refer to different repositories",
                ));
            }
        }
        repository.issue_id = Some(issue.id.clone());
        repository.issue_identifier = Some(issue.identifier.clone());
        repository.issue_url = issue.url.clone();
        Ok(repository)
    }

    fn from_issue_identifier(identifier: &str) -> Result<Self> {
        let Some((repository, number)) = identifier.rsplit_once('#') else {
            return Err(completion_error(
                "completion_repository_invalid_issue",
                "issue identifier must include an owner/repository prefix",
            ));
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(completion_error(
                "completion_repository_invalid_issue",
                "issue identifier must end with a numeric issue number",
            ));
        }
        Self::from_owner_name(repository)
    }

    fn from_issue_url(url: &str) -> Result<Self> {
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(completion_error(
                "completion_repository_invalid_issue_url",
                "issue URL must be an HTTPS GitHub URL",
            ));
        };
        let Some((host, path)) = rest.split_once('/') else {
            return Err(completion_error(
                "completion_repository_invalid_issue_url",
                "issue URL must include a GitHub repository path",
            ));
        };
        if !host.eq_ignore_ascii_case("github.com") || host.contains('@') {
            return Err(completion_error(
                "completion_repository_invalid_issue_url",
                "issue URL must use github.com without userinfo",
            ));
        }
        let mut segments = path.split('/');
        let (Some(owner), Some(name), Some(issues), Some(number)) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(completion_error(
                "completion_repository_invalid_issue_url",
                "issue URL must include owner, repository, and issue number",
            ));
        };
        if issues != "issues"
            || number.is_empty()
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || segments.next().is_some()
        {
            return Err(completion_error(
                "completion_repository_invalid_issue_url",
                "issue URL is not a canonical GitHub issue URL",
            ));
        }
        Self::from_owner_name(&format!("{owner}/{name}"))
    }

    fn from_owner_name(repository: &str) -> Result<Self> {
        let Some((owner, name)) = repository.split_once('/') else {
            return Err(completion_error(
                "completion_repository_invalid_issue",
                "issue repository must be owner/name",
            ));
        };
        if repository.matches('/').count() != 1
            || !is_github_repository_component(owner)
            || !is_github_repository_component(name)
        {
            return Err(completion_error(
                "completion_repository_invalid_issue",
                "issue repository contains an invalid owner or name",
            ));
        }
        Ok(Self {
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
            issue_id: None,
            issue_identifier: None,
            issue_url: None,
        })
    }

    fn pinned_https_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
    }

    pub fn matches_remote_url(&self, remote: &str) -> bool {
        normalize_github_remote(remote).is_some_and(|repository| self.same_identity(&repository))
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.owner == other.owner && self.name == other.name
    }

    fn ensure_for_issue(&self, issue: &Issue) -> Result<()> {
        if self.issue_id.as_deref() == Some(issue.id.as_str())
            && self.issue_identifier.as_deref() == Some(issue.identifier.as_str())
            && self.issue_url.as_deref() == issue.url.as_deref()
        {
            return Ok(());
        }
        Err(completion_error(
            "completion_repository_mismatch",
            "expected repository is not bound to the current issue",
        ))
    }
}

impl DirectCommitCompletion {
    pub fn new(
        config: &EffectiveConfig,
        writer: Option<Arc<dyn TrackerWriter>>,
    ) -> Result<Option<Self>> {
        if !config.completion.direct_commit.enabled {
            return Ok(None);
        }
        let writer = writer.ok_or_else(|| {
            completion_error(
                "completion_writer_unavailable",
                "direct-commit completion requires a tracker writer",
            )
        })?;
        config.tracker.github_endpoint_url()?;
        let token = config
            .tracker
            .api_key
            .clone()
            .filter(|token| !token.is_empty())
            .ok_or(SymphonyError::MissingTrackerApiKey)?;
        Ok(Some(Self {
            direct_commit: config.completion.direct_commit.clone(),
            token,
            writer,
            test_authenticated_remote_url: None,
        }))
    }

    #[doc(hidden)]
    pub fn new_with_test_authenticated_remote_url(
        config: &EffectiveConfig,
        writer: Option<Arc<dyn TrackerWriter>>,
        remote_url: String,
    ) -> Result<Option<Self>> {
        let Some(mut completion) = Self::new(config, writer)? else {
            return Ok(None);
        };
        completion.test_authenticated_remote_url = Some(remote_url);
        Ok(Some(completion))
    }

    pub async fn complete_issue(
        &self,
        issue: &Issue,
        workspace: &Path,
        expected_repository: &ExpectedGithubRepository,
    ) -> Result<CompletionResult> {
        expected_repository.ensure_for_issue(issue)?;
        let severity = Severity::from_issue(issue)?;
        let detected_changes = git_worktree_changes(workspace).await?;
        let has_workspace_changes = !detected_changes.is_empty();
        let has_pending_local_commits = if has_workspace_changes {
            true
        } else {
            git_status_has_unpushed_commits(workspace).await?
        };
        let target_state = severity.target_state(&self.direct_commit).to_string();

        if self.direct_commit.dry_run {
            return self
                .dry_run_plan(
                    issue,
                    workspace,
                    DryRunPlanInput {
                        severity,
                        target_state,
                        detected_changes,
                        has_workspace_changes,
                        has_pending_local_commits,
                    },
                )
                .await;
        }

        if !has_pending_local_commits {
            info!(issue_id = %issue.id, issue_identifier = %issue.identifier, "completion_skipped_no_changes");
            return Ok(CompletionResult::skipped("no workspace changes"));
        }
        ensure_on_base_branch(workspace, &self.direct_commit.base_branch).await?;
        ensure_workspace_matches_expected_repository(workspace, expected_repository).await?;
        ensure_no_local_url_rewrite(workspace, &expected_repository.pinned_https_url()).await?;
        if has_workspace_changes {
            git_commit_all(workspace, issue, &self.direct_commit).await?;
        }
        let authenticated_remote_url = self.authenticated_remote_url(expected_repository);
        git_fetch_base_branch(
            workspace,
            &self.direct_commit.base_branch,
            &self.token,
            authenticated_remote_url.as_str(),
        )
        .await?;
        let has_unpushed_commits =
            git_has_unpushed_commits(workspace, &self.direct_commit.base_branch).await?;
        if !has_unpushed_commits {
            info!(issue_id = %issue.id, issue_identifier = %issue.identifier, "completion_skipped_no_changes");
            return Ok(CompletionResult::skipped("no workspace changes"));
        }
        git_rebase_onto_base_branch(
            workspace,
            &self.direct_commit.base_branch,
            issue,
            &self.direct_commit,
        )
        .await?;

        let commit_sha = git_commit_sha(workspace).await?;
        git_push_base_branch(
            workspace,
            &self.direct_commit.base_branch,
            &self.token,
            authenticated_remote_url.as_str(),
        )
        .await?;
        if let Err(error) = self.writer.move_issue_to_state(issue, &target_state).await {
            let message = error.to_string();
            warn!(
                issue_id = %issue.id,
                issue_identifier = %issue.identifier,
                severity = %severity.as_str(),
                commit_sha = %commit_sha,
                target_state = %target_state,
                error = %message,
                "completion_direct_commit_partial_failure"
            );
            return Ok(CompletionResult {
                commit_sha: Some(commit_sha.clone()),
                moved_to_state: None,
                severity: Some(severity.as_str().to_string()),
                skipped_reason: None,
                plan: None,
                partial_failure: Some(CompletionPartialFailure {
                    pushed_commit_sha: commit_sha,
                    target_state,
                    message,
                }),
            });
        }
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            severity = %severity.as_str(),
            commit_sha = %commit_sha,
            target_state = %target_state,
            "completion_direct_commit_ready"
        );
        Ok(CompletionResult {
            commit_sha: Some(commit_sha),
            moved_to_state: Some(target_state),
            severity: Some(severity.as_str().to_string()),
            skipped_reason: None,
            plan: None,
            partial_failure: None,
        })
    }

    fn authenticated_remote_url(&self, repository: &ExpectedGithubRepository) -> String {
        self.test_authenticated_remote_url
            .clone()
            .unwrap_or_else(|| repository.pinned_https_url())
    }

    async fn dry_run_plan(
        &self,
        issue: &Issue,
        workspace: &Path,
        input: DryRunPlanInput,
    ) -> Result<CompletionResult> {
        let DryRunPlanInput {
            severity,
            target_state,
            detected_changes,
            has_workspace_changes,
            has_pending_local_commits,
        } = input;
        ensure_on_base_branch(workspace, &self.direct_commit.base_branch).await?;
        let rebase_required = has_pending_local_commits
            && git_rebase_required(workspace, &self.direct_commit.base_branch).await?;
        let title = commit_title(issue);
        let body = commit_body(issue);
        let mut planned_mutations = Vec::new();
        if has_workspace_changes {
            planned_mutations.push(CompletionMutation::StageAllChanges);
            planned_mutations.push(CompletionMutation::Commit {
                title: title.clone(),
                body: body.clone(),
            });
        }
        if has_pending_local_commits {
            planned_mutations.push(CompletionMutation::FetchBaseBranch {
                base_branch: self.direct_commit.base_branch.clone(),
            });
            if rebase_required {
                planned_mutations.push(CompletionMutation::RebaseOntoBaseBranch {
                    base_branch: self.direct_commit.base_branch.clone(),
                });
            }
            planned_mutations.push(CompletionMutation::PushBaseBranch {
                base_branch: self.direct_commit.base_branch.clone(),
            });
            planned_mutations.push(CompletionMutation::MoveIssueToState {
                target_state: target_state.clone(),
            });
        }
        let plan = CompletionPlan {
            severity: severity.as_str().to_string(),
            target_state: target_state.clone(),
            detected_changes,
            commit_title: title,
            commit_body: body,
            rebase_required,
            planned_mutations,
        };
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            severity = %severity.as_str(),
            target_state = %target_state,
            rebase_required,
            "completion_direct_commit_dry_run"
        );
        Ok(CompletionResult {
            commit_sha: None,
            moved_to_state: None,
            severity: Some(severity.as_str().to_string()),
            skipped_reason: (!has_pending_local_commits)
                .then(|| "no workspace changes".to_string()),
            plan: Some(plan),
            partial_failure: None,
        })
    }

    pub async fn mark_issue_started(&self, issue: &Issue) -> Result<Option<String>> {
        let Some(started_state) = self.direct_commit.started_state.as_deref() else {
            return Ok(None);
        };
        if issue.state.eq_ignore_ascii_case(started_state) {
            return Ok(None);
        }
        self.writer
            .move_issue_to_state(issue, started_state)
            .await?;
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            from_state = %issue.state,
            state = %started_state,
            "issue_marked_started"
        );
        Ok(Some(started_state.to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    fn from_issue(issue: &Issue) -> Result<Self> {
        parse_severity_prefix(&issue.title).ok_or_else(|| {
            completion_error(
                "missing_issue_severity",
                "issue title must start with [Low], [Medium], [High], or [Critical]",
            )
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }

    fn target_state(self, config: &DirectCommitCompletionConfig) -> &str {
        match self {
            Self::Low | Self::Medium => &config.auto_approved_state,
            Self::High | Self::Critical => &config.high_review_state,
        }
    }
}

async fn git_worktree_changes(workspace: &Path) -> Result<Vec<String>> {
    let output = git_output(workspace, &["status", "--porcelain=v1"], None).await?;
    Ok(output.lines().map(str::to_owned).collect())
}

async fn git_has_changes(workspace: &Path) -> Result<bool> {
    Ok(!git_worktree_changes(workspace).await?.is_empty())
}

async fn git_status_has_unpushed_commits(workspace: &Path) -> Result<bool> {
    let output = git_output(workspace, &["status", "--porcelain=v1", "--branch"], None).await?;
    Ok(output
        .lines()
        .next()
        .is_some_and(|line| line.contains("[ahead ")))
}

async fn git_fetch_base_branch(
    workspace: &Path,
    base_branch: &str,
    token: &str,
    remote_url: &str,
) -> Result<()> {
    let refspec = format!("refs/heads/{base_branch}:refs/remotes/origin/{base_branch}");
    let authorization = GitAuthorization::new(token);
    git_output(
        workspace,
        &["fetch", remote_url, refspec.as_str()],
        Some(&authorization),
    )
    .await
    .map(|_| ())
}

async fn git_has_unpushed_commits(workspace: &Path, base_branch: &str) -> Result<bool> {
    let revision_range = format!("origin/{base_branch}..HEAD");
    let output = git_output(workspace, &["rev-list", "--count", &revision_range], None).await?;
    let count = output
        .trim()
        .parse::<u64>()
        .map_err(|err| completion_error("git_output", err.to_string()))?;
    Ok(count > 0)
}

async fn ensure_workspace_matches_expected_repository(
    workspace: &Path,
    expected_repository: &ExpectedGithubRepository,
) -> Result<()> {
    let remote_url = git_remote_url(workspace).await?;
    if expected_repository.matches_remote_url(&remote_url) {
        return Ok(());
    }
    Err(completion_error(
        "completion_repository_mismatch",
        format!(
            "workspace origin does not match expected GitHub repository {}/{}",
            expected_repository.owner, expected_repository.name
        ),
    ))
}

async fn git_remote_url(workspace: &Path) -> Result<String> {
    git_output(workspace, &["config", "--get", "remote.origin.url"], None)
        .await
        .map(|url| url.trim().to_string())
}

async fn ensure_no_local_url_rewrite(workspace: &Path, pinned_url: &str) -> Result<()> {
    let config = git_output(
        workspace,
        &["config", "--includes", "--show-scope", "--list"],
        None,
    )
    .await?;
    for line in config.lines() {
        let Some((scope, entry)) = line.split_once('\t') else {
            continue;
        };
        if !matches!(scope, "local" | "worktree") {
            continue;
        }
        let Some((key, replacement)) = entry.split_once('=') else {
            continue;
        };
        if key.starts_with("url.")
            && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof"))
            && !replacement.is_empty()
            && pinned_url.starts_with(replacement)
        {
            return Err(completion_error(
                "completion_repository_rewrite",
                "workspace URL rewrite could redirect the pinned GitHub repository",
            ));
        }
    }
    Ok(())
}

async fn git_rebase_required(workspace: &Path, base_branch: &str) -> Result<bool> {
    let revision_range = format!("HEAD..origin/{base_branch}");
    let output = git_output(workspace, &["rev-list", "--count", &revision_range], None).await?;
    let count = output
        .trim()
        .parse::<u64>()
        .map_err(|err| completion_error("git_output", err.to_string()))?;
    Ok(count > 0)
}

async fn git_rebase_onto_base_branch(
    workspace: &Path,
    base_branch: &str,
    issue: &Issue,
    config: &DirectCommitCompletionConfig,
) -> Result<()> {
    let upstream = format!("origin/{base_branch}");
    let error = match git_rebase(workspace, &upstream, config).await {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    if !is_dirty_rebase_error(&error) {
        let _ = git_output(workspace, &["rebase", "--abort"], None).await;
        return Err(error);
    }

    let stashed_changes = if git_has_changes(workspace).await? {
        git_stash_push_rebase_retry(workspace).await?;
        true
    } else {
        false
    };
    let _ = git_output(workspace, &["rebase", "--abort"], None).await;
    if stashed_changes {
        git_stash_pop(workspace).await?;
    }
    if git_has_changes(workspace).await? {
        git_commit_all(workspace, issue, config).await?;
    }
    git_rebase_or_abort(workspace, &upstream, config).await
}

async fn git_rebase(
    workspace: &Path,
    upstream: &str,
    config: &DirectCommitCompletionConfig,
) -> Result<()> {
    git_output_with_environment(
        workspace,
        &["rebase", upstream],
        None,
        &[
            ("GIT_AUTHOR_NAME", config.commit_author_name.as_str()),
            ("GIT_AUTHOR_EMAIL", config.commit_author_email.as_str()),
            ("GIT_COMMITTER_NAME", config.commit_author_name.as_str()),
            ("GIT_COMMITTER_EMAIL", config.commit_author_email.as_str()),
        ],
    )
    .await
    .map(|_| ())
}

async fn git_rebase_or_abort(
    workspace: &Path,
    upstream: &str,
    config: &DirectCommitCompletionConfig,
) -> Result<()> {
    match git_rebase(workspace, upstream, config).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = git_output(workspace, &["rebase", "--abort"], None).await;
            Err(error)
        }
    }
}
async fn git_stash_push_rebase_retry(workspace: &Path) -> Result<()> {
    git_output(
        workspace,
        &[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "symphony-rebase-retry",
        ],
        None,
    )
    .await
    .map(|_| ())
}

async fn git_stash_pop(workspace: &Path) -> Result<()> {
    git_output(workspace, &["stash", "pop"], None)
        .await
        .map(|_| ())
}

fn is_dirty_rebase_error(error: &SymphonyError) -> bool {
    let SymphonyError::Tracker { message, .. } = error else {
        return false;
    };
    message.contains("Your local changes")
        || message.contains("Please commit your changes or stash")
        || message.contains("cannot rebase: You have unstaged changes")
}

async fn ensure_on_base_branch(workspace: &Path, base_branch: &str) -> Result<()> {
    let branch = git_output(workspace, &["rev-parse", "--abbrev-ref", "HEAD"], None).await?;
    let branch = branch.trim();
    if branch == base_branch {
        return Ok(());
    }
    Err(completion_error(
        "git_wrong_branch",
        format!("workspace branch {branch} does not match configured base branch {base_branch}"),
    ))
}

async fn git_commit_all(
    workspace: &Path,
    issue: &Issue,
    config: &DirectCommitCompletionConfig,
) -> Result<()> {
    git_output(workspace, &["add", "-A"], None).await?;
    let user_name = format!("user.name={}", config.commit_author_name);
    let user_email = format!("user.email={}", config.commit_author_email);
    let title = commit_title(issue);
    let body = commit_body(issue);
    git_output(
        workspace,
        &[
            "-c",
            user_name.as_str(),
            "-c",
            user_email.as_str(),
            "commit",
            "-m",
            title.as_str(),
            "-m",
            body.as_str(),
        ],
        None,
    )
    .await
    .map(|_| ())
}

async fn git_commit_sha(workspace: &Path) -> Result<String> {
    git_output(workspace, &["rev-parse", "HEAD"], None)
        .await
        .map(|sha| sha.trim().to_string())
}

async fn git_push_base_branch(
    workspace: &Path,
    base_branch: &str,
    token: &str,
    remote_url: &str,
) -> Result<()> {
    let refspec = format!("HEAD:refs/heads/{base_branch}");
    let authorization = GitAuthorization::new(token);
    git_output(
        workspace,
        &["push", remote_url, refspec.as_str()],
        Some(&authorization),
    )
    .await
    .map(|_| ())
}

fn is_github_repository_component(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn normalize_github_remote(remote: &str) -> Option<ExpectedGithubRepository> {
    let remote = remote.trim();
    let path = if let Some(rest) = strip_ascii_prefix(remote, "https://") {
        let (authority, path) = rest.split_once('/')?;
        if !authority.eq_ignore_ascii_case("github.com") || authority.contains('@') {
            return None;
        }
        path
    } else if let Some(rest) = strip_ascii_prefix(remote, "ssh://") {
        let (authority, path) = rest.split_once('/')?;
        if !authority.eq_ignore_ascii_case("git@github.com") {
            return None;
        }
        path
    } else {
        strip_ascii_prefix(remote, "git@github.com:")?
    };
    let path = strip_ascii_suffix(path, ".git").unwrap_or(path);
    ExpectedGithubRepository::from_owner_name(path).ok()
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

fn strip_ascii_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let start = value.len().checked_sub(suffix.len())?;
    value
        .get(start..)
        .filter(|candidate| candidate.eq_ignore_ascii_case(suffix))
        .map(|_| &value[..start])
}
struct GitAuthorization {
    header: String,
    redactions: Vec<String>,
}

impl GitAuthorization {
    fn new(token: &str) -> Self {
        let credentials = format!("x-access-token:{token}");
        let encoded = BASE64_STANDARD.encode(&credentials);
        let header = format!("AUTHORIZATION: basic {encoded}");
        let encoded_header = BASE64_STANDARD.encode(&header);
        let url_safe_encoded = BASE64_URL_SAFE_NO_PAD.encode(&credentials);
        let url_encoded_header = percent_encode(&header);
        Self {
            header: header.clone(),
            redactions: vec![
                token.to_string(),
                credentials,
                header,
                encoded,
                url_safe_encoded,
                encoded_header,
                url_encoded_header,
            ],
        }
    }

    fn redact(&self, message: impl AsRef<str>) -> String {
        let mut redacted = message.as_ref().to_string();
        for secret in &self.redactions {
            if !secret.is_empty() {
                redacted = redacted.replace(secret, "[REDACTED]");
            }
        }
        redacted
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn trusted_empty_hooks_dir() -> Result<&'static Path> {
    static HOOKS_DIR: LazyLock<std::result::Result<PathBuf, String>> = LazyLock::new(|| {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for attempt in 0..64 {
            let path = env::temp_dir().join(format!(
                "symphony-empty-git-hooks-{}-{nonce}-{attempt}",
                process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("could not create a trusted empty Git hooks directory".to_string())
    });
    HOOKS_DIR
        .as_deref()
        .map_err(|message| completion_error("git_hooks_path", message))
}

async fn git_output(
    workspace: &Path,
    args: &[&str],
    authorization: Option<&GitAuthorization>,
) -> Result<String> {
    git_output_with_environment(workspace, args, authorization, &[]).await
}

async fn git_output_with_environment(
    workspace: &Path,
    args: &[&str],
    authorization: Option<&GitAuthorization>,
    environment: &[(&str, &str)],
) -> Result<String> {
    let hooks_dir = authorization
        .map(|_| trusted_empty_hooks_dir())
        .transpose()?;
    let mut command = Command::new("git");
    command
        .env_clear()
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0");
    command.envs(environment.iter().copied());
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    for key in ["SystemRoot", "SYSTEMROOT", "ComSpec", "COMSPEC"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    if let (Some(authorization), Some(hooks_dir)) = (authorization, hooks_dir) {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", hooks_dir)
            .env("GIT_CONFIG_KEY_1", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_1", &authorization.header);
    }
    let output = command.output().await.map_err(|error| {
        let message = authorization.map_or_else(
            || error.to_string(),
            |authorization| authorization.redact(error.to_string()),
        );
        completion_error("git_spawn", message)
    })?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map_err(|error| completion_error("git_output", error.to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = match authorization {
        Some(authorization) => authorization.redact(&stderr),
        None => stderr.into_owned(),
    };
    Err(completion_error(
        "git_failed",
        format!("git {} failed: {stderr}", args.join(" ")),
    ))
}

fn parse_severity_prefix(title: &str) -> Option<Severity> {
    let title = title.trim_start();
    let rest = title.strip_prefix('[')?;
    let (severity, _) = rest.split_once(']')?;
    match severity.trim().to_ascii_lowercase().as_str() {
        "low" => Some(Severity::Low),
        "medium" => Some(Severity::Medium),
        "high" => Some(Severity::High),
        "critical" => Some(Severity::Critical),
        _ => None,
    }
}

fn commit_title(issue: &Issue) -> String {
    format!("{}: {}", issue.identifier, issue.title)
}

fn commit_body(issue: &Issue) -> String {
    let mut body = String::new();
    if let Some(url) = &issue.url {
        body.push_str("Issue: ");
        body.push_str(url);
        body.push_str("\n\n");
    }
    if let Some(number) = issue_number(issue) {
        body.push_str("Refs #");
        body.push_str(&number.to_string());
        body.push('\n');
    }
    body
}

fn issue_number(issue: &Issue) -> Option<i64> {
    issue
        .identifier
        .rsplit_once('#')
        .and_then(|(_, number)| number.parse::<i64>().ok())
}

fn completion_error(kind: &'static str, message: impl Into<String>) -> SymphonyError {
    SymphonyError::tracker(kind, message)
}

#[cfg(test)]
mod tests {
    use super::GitAuthorization;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

    #[test]
    fn authorization_redaction_removes_raw_and_encoded_forms() {
        let authorization = GitAuthorization::new("token-123");
        let failure = format!(
            "token-123 {} {} {}",
            authorization.header,
            BASE64_STANDARD.encode(&authorization.header),
            super::percent_encode(&authorization.header),
        );

        let redacted = authorization.redact(failure);

        assert!(!redacted.contains("token-123"));
        assert!(!redacted.contains(&authorization.header));
        assert!(!redacted.contains("eC1hY2Nlc3MtdG9rZW46dG9rZW4tMTIz"));
        assert!(!redacted.contains(&super::percent_encode(&authorization.header)));
    }
}
