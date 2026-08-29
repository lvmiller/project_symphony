use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::agent::codex::CodexClient;
use crate::completion::GitHubCompletionClient;
use crate::config::EffectiveConfig;
use crate::domain::{CodexEvent, Issue, WorkerExitReason};
use crate::error::Result;
use crate::prompt::{PromptSourceContext, continuation_prompt, render_prompt_with_source};
use crate::tracker::TrackerClient;
use crate::workspace::WorkspaceManager;

#[derive(Clone, Debug)]
pub struct WorkerOutcome {
    pub issue_id: String,
    pub reason: WorkerExitReason,
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
    codex: Arc<dyn CodexClient>,
}

impl SymphonyAgentRunner {
    pub fn new(
        config: EffectiveConfig,
        workspace: WorkspaceManager,
        tracker: Arc<dyn TrackerClient>,
        codex: Arc<dyn CodexClient>,
    ) -> Self {
        Self {
            config,
            workspace,
            tracker,
            codex,
        }
    }

    async fn run_inner(
        &self,
        issue: &Issue,
        attempt: Option<u32>,
        on_event: &mut (dyn FnMut(CodexEvent) + Send),
    ) -> WorkerExitReason {
        let workspace = match self
            .workspace
            .create_for_source_issue(&self.config.source.id, issue)
            .await
        {
            Ok(workspace) => workspace,
            Err(error) => return WorkerExitReason::Failed(error.to_string()),
        };
        let mut working_issue = issue.clone();
        if let Err(error) = self.mark_started_if_configured(&mut working_issue).await {
            return WorkerExitReason::Failed(error.to_string());
        }

        let mut reason = match self
            .run_in_workspace(&working_issue, attempt, on_event, &workspace.path)
            .await
        {
            Ok(reason) => reason,
            Err(error) => WorkerExitReason::Failed(error.to_string()),
        };

        self.workspace
            .after_run_best_effort_for_source(&self.config.source.id, &workspace.path)
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
                }
            }
        }
        reason
    }

    async fn mark_started_if_configured(&self, issue: &mut Issue) -> Result<()> {
        let Some(completion) = GitHubCompletionClient::new(&self.config)? else {
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
        let Some(completion) = GitHubCompletionClient::new(&self.config)? else {
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
        let result = completion.complete_issue(current, workspace_path).await?;
        Ok(result.commit_sha.is_some()
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
    ) -> Result<WorkerExitReason> {
        info!(
            source_id = %self.config.source.id,
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            workspace = %workspace_path.display(),
            "worker_starting"
        );
        self.workspace
            .before_run_for_source(&self.config.source.id, workspace_path)
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
                        return Ok(WorkerExitReason::Normal);
                    }
                    if !self.config.is_active_state(&current.state) {
                        warn!(
                            issue_id = %issue.id,
                            issue_identifier = %issue.identifier,
                            state = %current.state,
                            "worker_issue_no_longer_active"
                        );
                        return Ok(WorkerExitReason::CanceledByReconciliation);
                    }
                }
            }

            Ok(WorkerExitReason::Normal)
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
        let reason = self.run_inner(&issue, attempt, &mut *on_event).await;
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
        Ok(WorkerOutcome { issue_id, reason })
    }
}
