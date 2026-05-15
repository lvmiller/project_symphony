use std::path::Path;
use std::process::Stdio;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::info;

use crate::config::{
    DirectCommitCompletionConfig, EffectiveConfig, GithubConfig, GithubProjectOwnerType,
};
use crate::domain::Issue;
use crate::error::{Result, SymphonyError};

const PROJECT_STATUS_QUERY: &str = r#"
query SymphonyCompletionProject(
  $issueId: ID!
  $projectOwnerLogin: String!
  $projectNumber: Int!
  $isOrganization: Boolean!
  $isUser: Boolean!
) {
  organization(login: $projectOwnerLogin) @include(if: $isOrganization) {
    projectV2(number: $projectNumber) {
      id
      fields(first: 100) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options { id name }
          }
        }
      }
    }
  }
  user(login: $projectOwnerLogin) @include(if: $isUser) {
    projectV2(number: $projectNumber) {
      id
      fields(first: 100) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options { id name }
          }
        }
      }
    }
  }
  node(id: $issueId) {
    ... on Issue {
      projectItems(first: 100) {
        nodes {
          id
          project {
            id
            number
            owner {
              __typename
              ... on User { login }
              ... on Organization { login }
            }
          }
        }
      }
    }
  }
}
"#;

const UPDATE_PROJECT_STATUS_MUTATION: &str = r#"
mutation SymphonyUpdateCompletionStatus(
  $projectId: ID!
  $itemId: ID!
  $fieldId: ID!
  $optionId: String!
) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $projectId,
    itemId: $itemId,
    fieldId: $fieldId,
    value: { singleSelectOptionId: $optionId }
  }) {
    projectV2Item { id }
  }
}
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResult {
    pub commit_sha: Option<String>,
    pub moved_to_state: Option<String>,
    pub severity: Option<String>,
    pub skipped_reason: Option<String>,
}

impl CompletionResult {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            commit_sha: None,
            moved_to_state: None,
            severity: None,
            skipped_reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitHubCompletionClient {
    http: reqwest::Client,
    graphql_endpoint: String,
    token: String,
    github: GithubConfig,
    direct_commit: DirectCommitCompletionConfig,
}

impl GitHubCompletionClient {
    pub fn new(config: &EffectiveConfig) -> Result<Option<Self>> {
        if !config.completion.direct_commit.enabled {
            return Ok(None);
        }
        let github = config
            .tracker
            .github
            .clone()
            .ok_or(SymphonyError::MissingGithubConfig { field: "github" })?;
        let token = config
            .tracker
            .api_key
            .clone()
            .ok_or(SymphonyError::MissingTrackerApiKey)?;
        if token.is_empty() {
            return Err(SymphonyError::MissingTrackerApiKey);
        }
        let http = reqwest::Client::builder()
            .user_agent("symphony-rust-runtime")
            .build()
            .map_err(|err| completion_error("github_transport", err.to_string()))?;
        Ok(Some(Self {
            http,
            graphql_endpoint: config.tracker.endpoint.clone(),
            token,
            github,
            direct_commit: config.completion.direct_commit.clone(),
        }))
    }

    pub async fn complete_issue(
        &self,
        issue: &Issue,
        workspace: &Path,
    ) -> Result<CompletionResult> {
        let severity = Severity::from_issue(issue)?;
        let has_workspace_changes = git_has_changes(workspace).await?;
        let has_unpushed_commits = git_has_unpushed_commits(workspace).await?;
        if !has_workspace_changes && !has_unpushed_commits {
            info!(issue_id = %issue.id, issue_identifier = %issue.identifier, "completion_skipped_no_changes");
            return Ok(CompletionResult::skipped("no workspace changes"));
        }

        ensure_on_base_branch(workspace, &self.direct_commit.base_branch).await?;
        if has_workspace_changes {
            git_commit_all(workspace, issue, &self.direct_commit).await?;
        }
        let commit_sha = git_commit_sha(workspace).await?;
        git_push_base_branch(workspace, &self.direct_commit.base_branch, &self.token).await?;
        let target_state = severity.target_state(&self.direct_commit).to_string();
        self.move_issue_to_state(issue, &target_state).await?;
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            severity = %severity.as_str(),
            commit_sha = %commit_sha,
            state = %target_state,
            "completion_direct_commit_ready"
        );
        Ok(CompletionResult {
            commit_sha: Some(commit_sha),
            moved_to_state: Some(target_state),
            severity: Some(severity.as_str().to_string()),
            skipped_reason: None,
        })
    }

    pub async fn mark_issue_started(&self, issue: &Issue) -> Result<Option<String>> {
        let Some(started_state) = self.direct_commit.started_state.as_deref() else {
            return Ok(None);
        };
        if issue.state.eq_ignore_ascii_case(started_state) {
            return Ok(None);
        }
        self.move_issue_to_state(issue, started_state).await?;
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            from_state = %issue.state,
            state = %started_state,
            "issue_marked_started"
        );
        Ok(Some(started_state.to_string()))
    }

    async fn move_issue_to_state(&self, issue: &Issue, target_state: &str) -> Result<()> {
        let data = self
            .graphql(
                PROJECT_STATUS_QUERY,
                json!({
                    "issueId": issue.id,
                    "projectOwnerLogin": self.github.project_owner_login,
                    "projectNumber": self.github.project_number,
                    "isOrganization": matches!(self.github.project_owner_type, GithubProjectOwnerType::Organization),
                    "isUser": matches!(self.github.project_owner_type, GithubProjectOwnerType::User),
                }),
            )
            .await?;
        let owner = project_owner_data(&data, self.github.project_owner_type)?;
        let project = owner
            .get("projectV2")
            .and_then(Value::as_object)
            .ok_or_else(|| completion_error("github_malformed", "missing GitHub projectV2"))?;
        let project_id = project
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| completion_error("github_malformed", "missing GitHub project id"))?;
        let (field_id, option_id) = status_field_and_option(
            project.get("fields"),
            &self.github.status_field_name,
            target_state,
        )?;
        let item_id = project_item_id(&data, project_id, &self.github)?;
        self.graphql(
            UPDATE_PROJECT_STATUS_MUTATION,
            json!({
                "projectId": project_id,
                "itemId": item_id,
                "fieldId": field_id,
                "optionId": option_id,
            }),
        )
        .await?;
        Ok(())
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let response = self
            .http
            .post(&self.graphql_endpoint)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|err| completion_error("github_transport", err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| completion_error("github_transport", err.to_string()))?;
        if status != StatusCode::OK {
            return Err(completion_error(
                "github_status",
                format!("GitHub GraphQL HTTP status {}", status.as_u16()),
            ));
        }
        let envelope: GraphqlEnvelope = serde_json::from_str(&body)
            .map_err(|err| completion_error("github_malformed", err.to_string()))?;
        if let Some(errors) = envelope.errors
            && !errors.is_empty()
        {
            let message = errors
                .iter()
                .filter_map(|error| error.message.as_deref())
                .next()
                .unwrap_or("GitHub GraphQL error");
            return Err(completion_error("github_graphql", message));
        }
        envelope
            .data
            .ok_or_else(|| completion_error("github_malformed", "missing GraphQL data"))
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

#[derive(Debug, Deserialize)]
struct GraphqlEnvelope {
    data: Option<Value>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: Option<String>,
}

async fn git_has_changes(workspace: &Path) -> Result<bool> {
    let output = git_output(workspace, &["status", "--porcelain=v1"], None).await?;
    Ok(!output.trim().is_empty())
}

async fn git_has_unpushed_commits(workspace: &Path) -> Result<bool> {
    let output = git_output(workspace, &["status", "--porcelain=v1", "--branch"], None).await?;
    Ok(output
        .lines()
        .next()
        .is_some_and(|line| line.contains("[ahead ")))
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

async fn git_push_base_branch(workspace: &Path, base_branch: &str, token: &str) -> Result<()> {
    let refspec = format!("HEAD:refs/heads/{base_branch}");
    let auth_header = github_git_authorization_header(token);
    git_output(
        workspace,
        &["push", "origin", &refspec],
        Some(auth_header.as_str()),
    )
    .await
    .map(|_| ())
}

fn github_git_authorization_header(token: &str) -> String {
    let mut credentials = String::with_capacity("x-access-token:".len() + token.len());
    credentials.push_str("x-access-token:");
    credentials.push_str(token);

    let encoded = BASE64_STANDARD.encode(credentials);
    let mut header = String::with_capacity("AUTHORIZATION: basic ".len() + encoded.len());
    header.push_str("AUTHORIZATION: basic ");
    header.push_str(&encoded);
    header
}
async fn git_output(workspace: &Path, args: &[&str], auth_header: Option<&str>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(header) = auth_header {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_0", header);
    }
    let output = command
        .output()
        .await
        .map_err(|err| SymphonyError::io(Some(workspace.to_path_buf()), err))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map_err(|err| completion_error("git_output", err.to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(completion_error(
        "git_failed",
        format!("git {} failed: {stderr}", args.join(" ")),
    ))
}

fn project_owner_data(
    data: &Value,
    owner_type: GithubProjectOwnerType,
) -> Result<&serde_json::Map<String, Value>> {
    match owner_type {
        GithubProjectOwnerType::Organization => data.get("organization"),
        GithubProjectOwnerType::User => data.get("user"),
    }
    .and_then(Value::as_object)
    .ok_or_else(|| completion_error("github_malformed", "missing GitHub project owner"))
}

fn status_field_and_option(
    fields: Option<&Value>,
    status_field_name: &str,
    target_state: &str,
) -> Result<(String, String)> {
    let nodes = fields
        .and_then(|fields| fields.get("nodes"))
        .and_then(Value::as_array)
        .ok_or_else(|| completion_error("github_malformed", "missing GitHub project fields"))?;
    for field in nodes {
        let Some(field_name) = field.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !field_name.eq_ignore_ascii_case(status_field_name) {
            continue;
        }
        let field_id = field
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| completion_error("github_malformed", "missing Status field id"))?;
        let options = field
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| completion_error("github_malformed", "missing Status field options"))?;
        for option in options {
            let Some(option_name) = option.get("name").and_then(Value::as_str) else {
                continue;
            };
            if option_name.eq_ignore_ascii_case(target_state) {
                let option_id = option.get("id").and_then(Value::as_str).ok_or_else(|| {
                    completion_error("github_malformed", "missing target Status option id")
                })?;
                return Ok((field_id.to_string(), option_id.to_string()));
            }
        }
        return Err(completion_error(
            "github_malformed",
            format!("missing target status option {target_state}"),
        ));
    }
    Err(completion_error(
        "github_malformed",
        format!("missing Status field {status_field_name}"),
    ))
}

fn project_item_id(data: &Value, project_id: &str, github: &GithubConfig) -> Result<String> {
    let nodes = data
        .get("node")
        .and_then(|node| node.get("projectItems"))
        .and_then(|items| items.get("nodes"))
        .and_then(Value::as_array)
        .ok_or_else(|| completion_error("github_malformed", "missing issue project items"))?;
    for item in nodes {
        let Some(project) = item.get("project") else {
            continue;
        };
        let id_matches = project.get("id").and_then(Value::as_str) == Some(project_id);
        let owner_matches = project_owner_matches(project, github);
        let number_matches =
            project.get("number").and_then(Value::as_i64) == Some(github.project_number);
        if id_matches || (owner_matches && number_matches) {
            return item
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| completion_error("github_malformed", "missing project item id"));
        }
    }
    Err(completion_error(
        "github_malformed",
        "issue is not in the configured Project v2",
    ))
}

fn project_owner_matches(project: &Value, github: &GithubConfig) -> bool {
    let Some(owner) = project.get("owner") else {
        return false;
    };
    let login_matches =
        owner.get("login").and_then(Value::as_str) == Some(github.project_owner_login.as_str());
    let type_matches = match github.project_owner_type {
        GithubProjectOwnerType::Organization => {
            owner.get("__typename").and_then(Value::as_str) == Some("Organization")
        }
        GithubProjectOwnerType::User => {
            owner.get("__typename").and_then(Value::as_str) == Some("User")
        }
    };
    login_matches && type_matches
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
    use super::github_git_authorization_header;

    #[test]
    fn github_git_authorization_header_uses_x_access_token_basic_auth() {
        assert_eq!(
            github_git_authorization_header("token-123"),
            "AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46dG9rZW4tMTIz"
        );
    }
}
