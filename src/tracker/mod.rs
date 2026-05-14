use async_trait::async_trait;

use crate::domain::Issue;
use crate::error::Result;

pub mod github;

#[async_trait]
pub trait TrackerClient: Send + Sync {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>>;
    async fn fetch_issues_by_states(&self, state_names: &[String]) -> Result<Vec<Issue>>;
    async fn fetch_issue_states_by_ids(&self, issue_ids: &[String]) -> Result<Vec<Issue>>;
}
