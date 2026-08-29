use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::agent::codex::CodexClient;
use crate::completion::{DirectCommitCompletion, ExpectedGithubRepository};
use crate::config::EffectiveConfig;
use crate::domain::{CodexEvent, Issue, WorkerExitReason};
use crate::error::Result;
use crate::prompt::{PromptSourceContext, continuation_prompt, render_prompt_with_source};
use crate::tracker::{TrackerClient, TrackerWriter};
use crate::workspace::WorkspaceManager;

#[derive(Clone, Debug)]
pub struct WorkerOutcome {
    pub issue_id: String,
    pub reason: WorkerExitReason,
    /// State returned by the post-turn refresh when it made this worker stop.
    ///
    /// A normal exit with no terminal state is a continuation candidate.
    pub terminal_state: Option<String>,
}

#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(
        &self,
        issue: Issue,
        attempt: Option<u32>,
        on_event: Box<dyn FnMut(CodexEvent) + Send>,
    ) -> Result<WorkerOutcome>;
}

pub struct SymphonyAgentRunner {
    config: EffectiveConfig,
    workspace: WorkspaceManager,
    tracker: Arc<dyn TrackerClient>,
    writer: Option<Arc<dyn TrackerWriter>>,
    codex: Arc<dyn CodexClient>,
    test_authenticated_remote_url: Option<String>,
}

impl SymphonyAgentRunner {
    pub fn new(
        config: EffectiveConfig,
        workspace: WorkspaceManager,
        tracker: Arc<dyn TrackerClient>,
        writer: Option<Arc<dyn TrackerWriter>>,
        codex: Arc<dyn CodexClient>,
    ) -> Self {
        Self {
            config,
            workspace,
            tracker,
            writer,
            codex,
            test_authenticated_remote_url: None,
        }
    }

    #[doc(hidden)]
    pub fn new_with_test_authenticated_remote_url(
        config: EffectiveConfig,
        workspace: WorkspaceManager,
        tracker: Arc<dyn TrackerClient>,
        writer: Option<Arc<dyn TrackerWriter>>,
        codex: Arc<dyn CodexClient>,
        remote_url: String,
    ) -> Self {
        let mut runner = Self::new(config, workspace, tracker, writer, codex);
        runner.test_authenticated_remote_url = Some(remote_url);
        runner
    }

    async fn run_inner(
        &self,
        issue: &Issue,
        attempt: Option<u32>,
        on_event: &mut (dyn FnMut(CodexEvent) + Send),
    ) -> (WorkerExitReason, Option<String>) {
        let workspace = match self
            .workspace
            .create_for_source_issue(&self.config.source.id, issue)
            .await
        {
            Ok(workspace) => workspace,
            Err(error) => return (WorkerExitReason::Failed(error.to_string()), None),
        };
        let mut working_issue = issue.clone();
        if let Err(error) = self.mark_started_if_configured(&mut working_issue).await {
            return (WorkerExitReason::Failed(error.to_string()), None);
        }

        let (mut reason, mut terminal_state) = match self
            .run_in_workspace(&working_issue, attempt, on_event, &workspace.path)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => (WorkerExitReason::Failed(error.to_string()), None),
        };

        self.workspace
            .after_run_best_effort_for_source_issue(
                &self.config.source.id,
                &working_issue,
                &workspace.path,
            )
            .await;
        if reason.is_normal() {
            match self
                .complete_if_configured(&working_issue, &workspace.path)
                .await
            {
                Ok(true) => {
                    if let Err(error) = self
                        .workspace
                        .remove_for_source_issue(&self.config.source.id, &working_issue)
                        .await
                    {
                        warn!(
                            source_id = %self.config.source.id,
                            issue_id = %working_issue.id,
                            issue_identifier = %working_issue.identifier,
                            error = %error,
                            "workspace_cleanup_after_commit_failed"
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    reason = WorkerExitReason::Failed(error.to_string());
                    terminal_state = None;
                }
            }
        }
        (reason, terminal_state)
    }

    async fn mark_started_if_configured(&self, issue: &mut Issue) -> Result<()> {
        let Some(completion) = DirectCommitCompletion::new(&self.config, self.writer.clone())?
        else {
            return Ok(());
        };
        if let Some(started_state) = completion.mark_issue_started(issue).await? {
            issue.state = started_state;
        }
        Ok(())
    }

    async fn complete_if_configured(
        &self,
        issue: &Issue,
        workspace_path: &std::path::Path,
    ) -> Result<bool> {
        let completion = match self.test_authenticated_remote_url.as_ref() {
            Some(remote_url) => DirectCommitCompletion::new_with_test_authenticated_remote_url(
                &self.config,
                self.writer.clone(),
                remote_url.clone(),
            )?,
            None => DirectCommitCompletion::new(&self.config, self.writer.clone())?,
        };
        let Some(completion) = completion else {
            return Ok(false);
        };
        let refreshed = self
            .tracker
            .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id))
            .await?;
        let Some(current) = refreshed.iter().find(|current| current.id == issue.id) else {
            return Err(crate::error::SymphonyError::tracker(
                "completion_missing_issue",
                "issue was not returned by tracker state refresh",
            ));
        };
        if !self.config.is_active_state(&current.state) {
            info!(
                issue_id = %issue.id,
                issue_identifier = %issue.identifier,
                state = %current.state,
                "completion_skipped_inactive_issue"
            );
            return Ok(false);
        }
        let expected_repository =
            ExpectedGithubRepository::from_configured_issue(&self.config, current)?;
        let result = completion
            .complete_issue(current, workspace_path, &expected_repository)
            .await?;
        if let Some(partial_failure) = result.partial_failure {
            return Err(crate::error::SymphonyError::tracker(
                "completion_partial_failure",
                format!(
                    "pushed_commit_sha={} target_state={} message={}",
                    partial_failure.pushed_commit_sha,
                    partial_failure.target_state,
                    partial_failure.message
                ),
            ));
        }
        Ok(result.is_committed_success()
            && self
                .config
                .workspace
                .cleanup
                .removes_after_committed_success())
    }

    async fn run_in_workspace(
        &self,
        issue: &Issue,
        attempt: Option<u32>,
        on_event: &mut (dyn FnMut(CodexEvent) + Send),
        workspace_path: &std::path::Path,
    ) -> Result<(WorkerExitReason, Option<String>)> {
        info!(
            source_id = %self.config.source.id,
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            workspace = %workspace_path.display(),
            "worker_starting"
        );
        self.workspace
            .before_run_for_source_issue(&self.config.source.id, issue, workspace_path)
            .await?;

        let source = PromptSourceContext::from_config(&self.config);
        let first_prompt = render_prompt_with_source(
            self.config.prompt_template_or_default(),
            issue,
            attempt,
            &source,
        )?;
        let max_turns = self.config.agent.max_turns;
        let mut session = self.codex.start_session(workspace_path, on_event).await?;
        let result = async {
            for turn_index in 0..max_turns {
                let turn_number = turn_index + 1;
                let prompt: Cow<'_, str> = if turn_index == 0 {
                    Cow::Borrowed(first_prompt.as_str())
                } else {
                    Cow::Owned(continuation_prompt(attempt, turn_number, max_turns))
                };

                info!(
                    source_id = %self.config.source.id,
                    issue_id = %issue.id,
                    issue_identifier = %issue.identifier,
                    turn = turn_number,
                    max_turns,
                    "worker_turn_starting"
                );
                let turn = session.run_turn(prompt.as_ref()).await?;
                info!(
                    source_id = %self.config.source.id,
                    issue_id = %issue.id,
                    issue_identifier = %issue.identifier,
                    session_id = %turn.session_id,
                    thread_id = %turn.thread_id,
                    turn_id = %turn.turn_id,
                    turn = turn_number,
                    max_turns,
                    "worker_turn_completed"
                );

                let refreshed = self
                    .tracker
                    .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id))
                    .await?;
                if let Some(current) = refreshed.iter().find(|current| current.id == issue.id) {
                    if self.config.is_terminal_state(&current.state) {
                        info!(
                            issue_id = %issue.id,
                            issue_identifier = %issue.identifier,
                            state = %current.state,
                            "worker_issue_terminal"
                        );
                        return Ok((WorkerExitReason::Normal, Some(current.state.clone())));
                    }
                    if !self.config.is_active_state(&current.state) {
                        warn!(
                            issue_id = %issue.id,
                            issue_identifier = %issue.identifier,
                            state = %current.state,
                            "worker_issue_no_longer_active"
                        );
                        return Ok((WorkerExitReason::CanceledByReconciliation, None));
                    }
                }
            }

            Ok((WorkerExitReason::Normal, None))
        }
        .await;
        session.shutdown().await;
        result
    }
}

#[async_trait]
impl AgentRunner for SymphonyAgentRunner {
    async fn run(
        &self,
        issue: Issue,
        attempt: Option<u32>,
        mut on_event: Box<dyn FnMut(CodexEvent) + Send>,
    ) -> Result<WorkerOutcome> {
        let issue_id = issue.id.clone();
        let issue_identifier = issue.identifier.clone();
        let (reason, terminal_state) = self.run_inner(&issue, attempt, &mut *on_event).await;
        if reason.is_normal() {
            info!(issue_id = %issue_id, issue_identifier = %issue_identifier, "worker_completed");
        } else {
            warn!(
                issue_id = %issue_id,
                issue_identifier = %issue_identifier,
                reason = ?reason,
                "worker_failed"
            );
        }
        Ok(WorkerOutcome {
            issue_id,
            reason,
            terminal_state,
        })
    }
}
