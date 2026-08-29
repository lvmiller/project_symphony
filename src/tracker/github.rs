use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{EffectiveConfig, GithubConfig, GithubProjectOwnerType, TrackerConfig};
use crate::domain::{BlockerRef, Issue};
use crate::error::{Result, SymphonyError};
use crate::tracker::{TrackerClient, TrackerWriter};

const PROJECT_ITEMS_QUERY: &str = r#"
query SymphonyProjectItems(
  $repositoryOwner: String!
  $repositoryName: String!
  $projectOwnerLogin: String!
  $projectNumber: Int!
  $after: String
  $isOrganization: Boolean!
  $isUser: Boolean!
) {
  repository(owner: $repositoryOwner, name: $repositoryName) { id }
  organization(login: $projectOwnerLogin) @include(if: $isOrganization) {
    projectV2(number: $projectNumber) {
      items(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          content {
            __typename
            ... on Issue {
              id number title body url createdAt updatedAt
              repository { nameWithOwner name owner { login } }
              labels(first: 100) { pageInfo { hasNextPage endCursor } nodes { name } }
            }
          }
          fieldValues(first: 50) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on ProjectV2ItemFieldSingleSelectValue { name field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { name } } }
            }
          }
        }
      }
    }
  }
  user(login: $projectOwnerLogin) @include(if: $isUser) {
    projectV2(number: $projectNumber) {
      items(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          content {
            __typename
            ... on Issue {
              id number title body url createdAt updatedAt
              repository { nameWithOwner name owner { login } }
              labels(first: 100) { pageInfo { hasNextPage endCursor } nodes { name } }
            }
          }
          fieldValues(first: 50) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on ProjectV2ItemFieldSingleSelectValue { name field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { name } } }
            }
          }
        }
      }
    }
  }
}
"#;

const ISSUE_STATES_QUERY: &str = r#"
query SymphonyIssueStates($ids: [ID!]!) {
  nodes(ids: $ids) {
    __typename
    ... on Issue {
      id number title body url createdAt updatedAt
      repository { nameWithOwner name owner { login } }
      labels(first: 100) { pageInfo { hasNextPage endCursor } nodes { name } }
      projectItems(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          project {
            number
            owner {
              __typename
              ... on User { login }
              ... on Organization { login }
            }
          }
          fieldValues(first: 50) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on ProjectV2ItemFieldSingleSelectValue { name field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { name } } }
            }
          }
        }
      }
    }
  }
}
"#;

const ISSUE_LABELS_QUERY: &str = r#"
query SymphonyIssueLabels($id: ID!, $after: String) {
  node(id: $id) {
    __typename
    ... on Issue {
      labels(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { name }
      }
    }
  }
}
"#;

const PROJECT_ITEM_FIELD_VALUES_QUERY: &str = r#"
query SymphonyProjectItemFieldValues($id: ID!, $after: String) {
  node(id: $id) {
    __typename
    ... on ProjectV2Item {
      fieldValues(first: 50, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          __typename
          ... on ProjectV2ItemFieldSingleSelectValue { name field { ... on ProjectV2FieldCommon { name } } }
          ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { name } } }
          ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { name } } }
        }
      }
    }
  }
}
"#;

const ISSUE_PROJECT_ITEMS_QUERY: &str = r#"
query SymphonyIssueProjectItems($id: ID!, $after: String) {
  node(id: $id) {
    __typename
    ... on Issue {
      projectItems(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
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
          fieldValues(first: 50) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on ProjectV2ItemFieldSingleSelectValue { name field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { name } } }
              ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { name } } }
            }
          }
        }
      }
    }
  }
}
"#;

const PROJECT_STATUS_FIELDS_QUERY: &str = r#"
query SymphonyProjectStatusFields(
  $projectOwnerLogin: String!
  $projectNumber: Int!
  $after: String
  $isOrganization: Boolean!
  $isUser: Boolean!
) {
  organization(login: $projectOwnerLogin) @include(if: $isOrganization) {
    projectV2(number: $projectNumber) {
      id
      fields(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
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
      fields(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
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
}
"#;

const UPDATE_PROJECT_STATUS_MUTATION: &str = r#"
mutation SymphonyUpdateProjectStatus(
  $projectId: ID!
  $itemId: ID!
  $fieldId: ID!
  $optionId: String!
) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $projectId
    itemId: $itemId
    fieldId: $fieldId
    value: { singleSelectOptionId: $optionId }
  }) {
    projectV2Item { id }
  }
}
"#;

#[derive(Clone)]
pub struct GitHubGraphqlExecutor {
    http: reqwest::Client,
    endpoint: String,
    token: String,
}

impl fmt::Debug for GitHubGraphqlExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubGraphqlExecutor")
            .finish_non_exhaustive()
    }
}

impl GitHubGraphqlExecutor {
    pub fn from_tracker_config(tracker: &TrackerConfig) -> Result<Self> {
        let github = tracker
            .github
            .as_ref()
            .ok_or(SymphonyError::MissingGithubConfig { field: "github" })?;
        let token = tracker
            .api_key
            .clone()
            .ok_or(SymphonyError::MissingTrackerApiKey)?;
        if token.is_empty() {
            return Err(SymphonyError::MissingTrackerApiKey);
        }
        github.validate()?;
        let http = reqwest::Client::builder()
            .user_agent("symphony-rust-runtime")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| tracker_error("github_transport", err.to_string()))?;
        Ok(Self {
            http,
            endpoint: tracker.endpoint.clone(),
            token,
        })
    }

    pub async fn execute(&self, query: &str, variables: Value) -> Result<Value> {
        if !variables.is_object() {
            return Err(tracker_error(
                "github_malformed",
                "GraphQL variables must be a JSON object",
            ));
        }
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|err| tracker_error("github_transport", err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| tracker_error("github_transport", err.to_string()))?;
        if status != StatusCode::OK {
            return Err(tracker_error(
                "github_status",
                format!("GitHub GraphQL HTTP status {}", status.as_u16()),
            ));
        }
        let body: Value = serde_json::from_str(&body)
            .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
        if !body.is_object() {
            return Err(tracker_error(
                "github_malformed",
                "GitHub GraphQL response must be a JSON object",
            ));
        }
        Ok(body)
    }
}

#[derive(Clone, Debug)]
pub struct GitHubTrackerClient {
    graphql_executor: GitHubGraphqlExecutor,
    active_states: Vec<String>,
    github: GithubConfig,
}

impl GitHubTrackerClient {
    pub fn new(config: &EffectiveConfig) -> Result<Self> {
        Self::from_tracker_config(&config.tracker)
    }

    pub fn from_tracker_config(tracker: &TrackerConfig) -> Result<Self> {
        let github = tracker
            .github
            .clone()
            .ok_or(SymphonyError::MissingGithubConfig { field: "github" })?;
        Ok(Self {
            graphql_executor: GitHubGraphqlExecutor::from_tracker_config(tracker)?,
            active_states: tracker.active_states.clone(),
            github,
        })
    }

    pub fn graphql_executor(&self) -> GitHubGraphqlExecutor {
        self.graphql_executor.clone()
    }

    async fn fetch_project_issues_for_states(&self, states: &[String]) -> Result<Vec<Issue>> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted = normalized_set(states);
        let mut after: Option<String> = None;
        let mut issues = Vec::new();
        for _ in 0..1_000 {
            let data = self
                .graphql(
                    PROJECT_ITEMS_QUERY,
                    self.project_items_variables(after.as_deref()),
                )
                .await?;
            let connection = self.project_items_connection(&data)?;
            for item in connection.nodes {
                let item = self.complete_project_item(item).await?;
                if let Some(issue) = normalize_project_item(&self.github, item)?
                    && wanted.contains(&normalize_state_name(&issue.state))
                {
                    issues.push(issue);
                }
            }
            if !connection.page_info.has_next_page {
                return Ok(issues);
            }
            let cursor = connection.page_info.end_cursor.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            if after.as_deref() == Some(cursor.as_str()) {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = Some(cursor);
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub project item pagination exceeded safety limit",
        ))
    }

    fn project_items_variables(&self, after: Option<&str>) -> Value {
        json!({
            "repositoryOwner": self.github.repository_owner,
            "repositoryName": self.github.repository_name,
            "projectOwnerLogin": self.github.project_owner_login,
            "projectNumber": self.github.project_number,
            "after": after,
            "isOrganization": matches!(self.github.project_owner_type, GithubProjectOwnerType::Organization),
            "isUser": matches!(self.github.project_owner_type, GithubProjectOwnerType::User),
        })
    }

    fn project_items_connection(&self, data: &Value) -> Result<ProjectItemsConnection> {
        let owner = match self.github.project_owner_type {
            GithubProjectOwnerType::Organization => data.get("organization"),
            GithubProjectOwnerType::User => data.get("user"),
        }
        .and_then(Value::as_object)
        .ok_or_else(|| tracker_error("github_malformed", "missing GitHub project owner"))?;
        let project = owner
            .get("projectV2")
            .and_then(Value::as_object)
            .ok_or_else(|| tracker_error("github_malformed", "missing GitHub projectV2"))?;
        let items = project
            .get("items")
            .ok_or_else(|| tracker_error("github_malformed", "missing GitHub project items"))?;
        serde_json::from_value(items.clone())
            .map_err(|err| tracker_error("github_malformed", err.to_string()))
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = self.graphql_executor.execute(query, variables).await?;
        let envelope: GraphqlEnvelope = serde_json::from_value(body)
            .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
        if let Some(errors) = envelope.errors
            && !errors.is_empty()
        {
            let message = errors
                .iter()
                .filter_map(|error| error.message.as_deref())
                .next()
                .unwrap_or("GitHub GraphQL error");
            return Err(tracker_error("github_graphql", message));
        }
        envelope
            .data
            .ok_or_else(|| tracker_error("github_malformed", "missing GraphQL data"))
    }

    async fn complete_project_item(&self, mut item: ProjectItem) -> Result<ProjectItem> {
        if item.field_values.page_info.has_next_page {
            let id = item.id.clone();
            self.append_project_item_field_values(&id, &mut item.field_values)
                .await?;
        }
        if let Some(content) = item.content.as_mut()
            && content.get("__typename").and_then(Value::as_str) == Some("Issue")
        {
            self.append_issue_labels(content).await?;
        }
        Ok(item)
    }

    async fn append_issue_labels(&self, issue: &mut Value) -> Result<()> {
        let issue_id = issue
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| tracker_error("github_malformed", "missing issue id"))?
            .to_string();
        let labels = issue
            .get_mut("labels")
            .ok_or_else(|| tracker_error("github_malformed", "missing issue labels"))?;
        let Some(connection) = labels.as_object_mut() else {
            return Err(tracker_error("github_malformed", "malformed issue labels"));
        };
        let mut page_info: PageInfo =
            serde_json::from_value(connection.get("pageInfo").cloned().ok_or_else(|| {
                tracker_error("github_malformed", "missing issue label pageInfo")
            })?)
            .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
        let mut after = page_info.end_cursor.clone();
        for _ in 0..1_000 {
            if !page_info.has_next_page {
                return Ok(());
            }
            let cursor = after.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            let next = self.fetch_issue_labels(&issue_id, Some(&cursor)).await?;
            let nodes: Vec<Value> = next
                .nodes
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            extend_connection_nodes(connection, &nodes)?;
            if next.page_info.end_cursor.as_deref() == Some(cursor.as_str())
                && next.page_info.has_next_page
            {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = next.page_info.end_cursor.clone();
            page_info = next.page_info;
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub issue label pagination exceeded safety limit",
        ))
    }

    async fn fetch_issue_labels(
        &self,
        issue_id: &str,
        after: Option<&str>,
    ) -> Result<LabelConnection> {
        let data = self
            .graphql(
                ISSUE_LABELS_QUERY,
                json!({ "id": issue_id, "after": after }),
            )
            .await?;
        let labels = data
            .get("node")
            .and_then(|node| node.get("labels"))
            .ok_or_else(|| tracker_error("github_malformed", "missing issue labels"))?;
        serde_json::from_value(labels.clone())
            .map_err(|err| tracker_error("github_malformed", err.to_string()))
    }

    async fn fetch_project_item_field_values(
        &self,
        item_id: &str,
        after: Option<&str>,
    ) -> Result<FieldValueConnection> {
        let data = self
            .graphql(
                PROJECT_ITEM_FIELD_VALUES_QUERY,
                json!({ "id": item_id, "after": after }),
            )
            .await?;
        let field_values = data
            .get("node")
            .and_then(|node| node.get("fieldValues"))
            .ok_or_else(|| tracker_error("github_malformed", "missing project item fieldValues"))?;
        serde_json::from_value(field_values.clone())
            .map_err(|err| tracker_error("github_malformed", err.to_string()))
    }

    async fn fetch_issue_project_items(
        &self,
        issue_id: &str,
        after: Option<&str>,
    ) -> Result<ProjectItemsConnection> {
        let data = self
            .graphql(
                ISSUE_PROJECT_ITEMS_QUERY,
                json!({ "id": issue_id, "after": after }),
            )
            .await?;
        let project_items = data
            .get("node")
            .and_then(|node| node.get("projectItems"))
            .ok_or_else(|| tracker_error("github_malformed", "missing issue projectItems"))?;
        serde_json::from_value(project_items.clone())
            .map_err(|err| tracker_error("github_malformed", err.to_string()))
    }

    async fn complete_issue_state_node(&self, issue: &mut Value) -> Result<bool> {
        self.append_issue_labels(issue).await?;
        self.append_issue_project_items(issue).await?;
        if !self.retain_configured_project_items(issue)? {
            return Ok(false);
        }
        self.append_issue_project_item_field_values(issue).await?;
        Ok(true)
    }

    async fn append_issue_project_items(&self, issue: &mut Value) -> Result<()> {
        let issue_id = issue
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| tracker_error("github_malformed", "missing issue id"))?
            .to_string();
        let project_items = issue
            .get_mut("projectItems")
            .ok_or_else(|| tracker_error("github_malformed", "missing issue projectItems"))?;
        let Some(connection) = project_items.as_object_mut() else {
            return Err(tracker_error(
                "github_malformed",
                "malformed issue projectItems",
            ));
        };
        let mut page_info: PageInfo =
            serde_json::from_value(connection.get("pageInfo").cloned().ok_or_else(|| {
                tracker_error("github_malformed", "missing issue project item pageInfo")
            })?)
            .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
        let mut after = page_info.end_cursor.clone();
        for _ in 0..1_000 {
            if !page_info.has_next_page {
                return Ok(());
            }
            let cursor = after.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            let next = self
                .fetch_issue_project_items(&issue_id, Some(&cursor))
                .await?;
            let nodes: Vec<Value> = next
                .nodes
                .into_iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            extend_connection_nodes(connection, &nodes)?;
            if next.page_info.end_cursor.as_deref() == Some(cursor.as_str())
                && next.page_info.has_next_page
            {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = next.page_info.end_cursor.clone();
            page_info = next.page_info;
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub issue project item pagination exceeded safety limit",
        ))
    }

    fn retain_configured_project_items(&self, issue: &mut Value) -> Result<bool> {
        let project_items = issue
            .get_mut("projectItems")
            .and_then(|project_items| project_items.get_mut("nodes"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| tracker_error("github_malformed", "missing issue projectItems"))?;
        let mut configured_items = Vec::new();
        for project_item in std::mem::take(project_items) {
            if project_item_matches_configured_project(&self.github, &project_item)? {
                configured_items.push(project_item);
            }
        }
        let is_member = !configured_items.is_empty();
        *project_items = configured_items;
        Ok(is_member)
    }

    async fn append_issue_project_item_field_values(&self, issue: &mut Value) -> Result<()> {
        let Some(project_items) = issue
            .get_mut("projectItems")
            .and_then(|project_items| project_items.get_mut("nodes"))
            .and_then(Value::as_array_mut)
        else {
            return Ok(());
        };
        for project_item in project_items {
            let item_id = project_item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| tracker_error("github_malformed", "missing project item id"))?
                .to_string();
            let field_values = project_item.get_mut("fieldValues").ok_or_else(|| {
                tracker_error("github_malformed", "missing project item fieldValues")
            })?;
            let mut connection: FieldValueConnection = serde_json::from_value(field_values.clone())
                .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            if connection.page_info.has_next_page {
                self.append_project_item_field_values(&item_id, &mut connection)
                    .await?;
                *field_values = serde_json::to_value(connection)
                    .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            }
        }
        Ok(())
    }

    async fn append_project_item_field_values(
        &self,
        item_id: &str,
        connection: &mut FieldValueConnection,
    ) -> Result<()> {
        let mut page_info = connection.page_info.clone();
        let mut after = page_info.end_cursor.clone();
        for _ in 0..1_000 {
            if !page_info.has_next_page {
                connection.page_info = page_info;
                return Ok(());
            }
            let cursor = after.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            let next = self
                .fetch_project_item_field_values(item_id, Some(&cursor))
                .await?;
            connection.nodes.extend(next.nodes);
            if next.page_info.end_cursor.as_deref() == Some(cursor.as_str())
                && next.page_info.has_next_page
            {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = next.page_info.end_cursor.clone();
            page_info = next.page_info;
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub project item fieldValues pagination exceeded safety limit",
        ))
    }
    async fn configured_project_status(
        &self,
        target_state: &str,
    ) -> Result<(String, String, String)> {
        let mut after: Option<String> = None;
        for _ in 0..1_000 {
            let data = self
                .graphql(
                    PROJECT_STATUS_FIELDS_QUERY,
                    json!({
                        "projectOwnerLogin": self.github.project_owner_login,
                        "projectNumber": self.github.project_number,
                        "after": after,
                        "isOrganization": matches!(self.github.project_owner_type, GithubProjectOwnerType::Organization),
                        "isUser": matches!(self.github.project_owner_type, GithubProjectOwnerType::User),
                    }),
                )
                .await?;
            let owner = match self.github.project_owner_type {
                GithubProjectOwnerType::Organization => data.get("organization"),
                GithubProjectOwnerType::User => data.get("user"),
            }
            .and_then(Value::as_object)
            .ok_or_else(|| tracker_error("github_malformed", "missing GitHub project owner"))?;
            let project = owner
                .get("projectV2")
                .and_then(Value::as_object)
                .ok_or_else(|| tracker_error("github_malformed", "missing GitHub projectV2"))?;
            let project_id = project
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| tracker_error("github_malformed", "missing GitHub project id"))?;
            let fields = project.get("fields").ok_or_else(|| {
                tracker_error("github_malformed", "missing GitHub project fields")
            })?;
            if let Some((field_id, option_id)) =
                status_field_and_option(fields, &self.github.status_field_name, target_state)?
            {
                return Ok((project_id.to_string(), field_id, option_id));
            }
            let page_info: PageInfo =
                serde_json::from_value(fields.get("pageInfo").cloned().ok_or_else(|| {
                    tracker_error("github_malformed", "missing project field pageInfo")
                })?)
                .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            if !page_info.has_next_page {
                return Err(tracker_error(
                    "github_malformed",
                    format!("missing Status field {}", self.github.status_field_name),
                ));
            }
            let cursor = page_info.end_cursor.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            if after.as_deref() == Some(cursor.as_str()) {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = Some(cursor);
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub project field pagination exceeded safety limit",
        ))
    }

    async fn configured_project_item_id(&self, issue_id: &str, project_id: &str) -> Result<String> {
        let mut after: Option<String> = None;
        for _ in 0..1_000 {
            let connection = self
                .fetch_issue_project_items(issue_id, after.as_deref())
                .await?;
            for item in connection.nodes {
                let Some(project) = item.project.as_ref() else {
                    continue;
                };
                if project.get("id").and_then(Value::as_str) == Some(project_id) {
                    return Ok(item.id);
                }
            }
            if !connection.page_info.has_next_page {
                return Err(tracker_error(
                    "github_malformed",
                    "issue is not in the configured Project v2",
                ));
            }
            let cursor = connection.page_info.end_cursor.ok_or_else(|| {
                tracker_error(
                    "github_pagination",
                    "GitHub returned hasNextPage=true without endCursor",
                )
            })?;
            if after.as_deref() == Some(cursor.as_str()) {
                return Err(tracker_error(
                    "github_pagination",
                    "GitHub returned the same pagination cursor twice",
                ));
            }
            after = Some(cursor);
        }
        Err(tracker_error(
            "github_pagination",
            "GitHub issue project item pagination exceeded safety limit",
        ))
    }
}

#[async_trait]
impl TrackerClient for GitHubTrackerClient {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>> {
        self.fetch_project_issues_for_states(&self.active_states)
            .await
    }

    async fn fetch_issues_by_states(&self, state_names: &[String]) -> Result<Vec<Issue>> {
        self.fetch_project_issues_for_states(state_names).await
    }

    async fn fetch_issue_states_by_ids(&self, issue_ids: &[String]) -> Result<Vec<Issue>> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        let data = self
            .graphql(ISSUE_STATES_QUERY, json!({ "ids": issue_ids }))
            .await?;
        let nodes = data
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| tracker_error("github_malformed", "missing GraphQL nodes"))?;
        let mut issues = Vec::new();
        for node in nodes {
            if node.is_null() {
                continue;
            }
            if node.get("__typename").and_then(Value::as_str) != Some("Issue") {
                continue;
            }
            let mut node = node.clone();
            if !self.complete_issue_state_node(&mut node).await? {
                continue;
            }
            let issue_node: IssueNode = serde_json::from_value(node.clone())
                .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
            let mut values = Vec::new();
            if let Some(project_items) = node
                .get("projectItems")
                .and_then(|project_items| project_items.get("nodes"))
                .and_then(Value::as_array)
            {
                for project_item in project_items {
                    if let Some(field_values) = project_item
                        .get("fieldValues")
                        .and_then(|field_values| field_values.get("nodes"))
                        .and_then(Value::as_array)
                    {
                        values.extend(field_values.iter().cloned());
                    }
                }
            }
            let state =
                field_value_by_name(&values, &self.github.status_field_name).ok_or_else(|| {
                    tracker_error(
                        "github_malformed",
                        "configured GitHub project item missing Status value",
                    )
                })?;
            issues.push(normalize_issue(&self.github, issue_node, &values, state)?);
        }
        Ok(issues)
    }
}

#[async_trait]
impl TrackerWriter for GitHubTrackerClient {
    async fn move_issue_to_state(&self, issue: &Issue, target_state: &str) -> Result<()> {
        let (project_id, field_id, option_id) =
            self.configured_project_status(target_state).await?;
        let item_id = self
            .configured_project_item_id(&issue.id, &project_id)
            .await?;
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectItemsConnection {
    page_info: PageInfo,
    nodes: Vec<ProjectItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectItem {
    id: String,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    project: Option<Value>,
    field_values: FieldValueConnection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldValueConnection {
    page_info: PageInfo,
    nodes: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    id: String,
    number: i64,
    title: String,
    body: Option<String>,
    url: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    repository: RepositoryNode,
    labels: LabelConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryNode {
    name_with_owner: Option<String>,
    name: String,
    owner: RepositoryOwnerNode,
}

#[derive(Debug, Deserialize)]
struct RepositoryOwnerNode {
    login: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LabelConnection {
    page_info: PageInfo,
    nodes: Vec<LabelNode>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LabelNode {
    name: String,
}

fn normalize_project_item(github: &GithubConfig, item: ProjectItem) -> Result<Option<Issue>> {
    let Some(content_value) = item.content else {
        return Ok(None);
    };
    if content_value.get("__typename").and_then(Value::as_str) != Some("Issue") {
        return Ok(None);
    }
    let content: IssueNode = serde_json::from_value(content_value)
        .map_err(|err| tracker_error("github_malformed", err.to_string()))?;
    if !issue_matches_configured_repository(github, &content) {
        return Ok(None);
    }
    let state = field_value_by_name(&item.field_values.nodes, &github.status_field_name)
        .ok_or_else(|| tracker_error("github_malformed", "missing project Status value"))?;
    normalize_issue(github, content, &item.field_values.nodes, state).map(Some)
}

fn issue_matches_configured_repository(github: &GithubConfig, content: &IssueNode) -> bool {
    github.issue_matches_configured_repository(
        &content.repository.owner.login,
        &content.repository.name,
    )
}

fn project_item_matches_configured_project(github: &GithubConfig, item: &Value) -> Result<bool> {
    let project = item
        .get("project")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            tracker_error("github_malformed", "missing project item project identity")
        })?;
    let number = project
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| tracker_error("github_malformed", "missing project item project number"))?;
    let owner = project
        .get("owner")
        .and_then(Value::as_object)
        .ok_or_else(|| tracker_error("github_malformed", "missing project item project owner"))?;
    let owner_type = owner
        .get("__typename")
        .and_then(Value::as_str)
        .ok_or_else(|| tracker_error("github_malformed", "missing project item owner type"))?;
    let owner_login = owner
        .get("login")
        .and_then(Value::as_str)
        .ok_or_else(|| tracker_error("github_malformed", "missing project item owner login"))?;
    let configured_owner_type = match github.project_owner_type {
        GithubProjectOwnerType::Organization => "Organization",
        GithubProjectOwnerType::User => "User",
    };
    Ok(number == github.project_number
        && owner_type == configured_owner_type
        && owner_login == github.project_owner_login)
}

fn status_field_and_option(
    fields: &Value,
    status_field_name: &str,
    target_state: &str,
) -> Result<Option<(String, String)>> {
    let nodes = fields
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| tracker_error("github_malformed", "missing GitHub project fields"))?;
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
            .ok_or_else(|| tracker_error("github_malformed", "missing Status field id"))?;
        let options = field
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| tracker_error("github_malformed", "missing Status field options"))?;
        for option in options {
            let Some(option_name) = option.get("name").and_then(Value::as_str) else {
                continue;
            };
            if option_name.eq_ignore_ascii_case(target_state) {
                let option_id = option.get("id").and_then(Value::as_str).ok_or_else(|| {
                    tracker_error("github_malformed", "missing target Status option id")
                })?;
                return Ok(Some((field_id.to_string(), option_id.to_string())));
            }
        }
        return Err(tracker_error(
            "github_malformed",
            format!("missing target status option {target_state}"),
        ));
    }
    Ok(None)
}

fn normalize_issue(
    github: &GithubConfig,
    content: IssueNode,
    field_values: &[Value],
    state: String,
) -> Result<Issue> {
    let labels: Vec<String> = content
        .labels
        .nodes
        .iter()
        .map(|label| label.name.to_ascii_lowercase())
        .collect();
    let priority = priority_from_field(github, field_values)
        .or_else(|| priority_from_labels(&labels, &github.priority_labels));
    let blocked_by = blockers_from_field(github, field_values)
        .or_else(|| blockers_from_labels(github, &labels))
        .unwrap_or_default();
    let name_with_owner = content.repository.name_with_owner.unwrap_or_else(|| {
        format!(
            "{}/{}",
            content.repository.owner.login, content.repository.name
        )
    });
    Ok(Issue {
        id: content.id,
        identifier: format!("{}#{}", name_with_owner, content.number),
        title: content.title,
        description: content.body,
        priority,
        state,
        branch_name: None,
        url: content.url,
        labels,
        blocked_by,
        created_at: content.created_at,
        updated_at: content.updated_at,
    })
}

fn field_value_by_name(field_values: &[Value], wanted_name: &str) -> Option<String> {
    field_values.iter().find_map(|field_value| {
        let field_name = field_value.get("field")?.get("name")?.as_str()?;
        if !field_name.eq_ignore_ascii_case(wanted_name) {
            return None;
        }
        field_value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| field_value.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| field_value.get("number").map(number_to_string))
    })
}

fn number_to_string(value: &Value) -> String {
    value
        .as_i64()
        .map(|number| number.to_string())
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .unwrap_or_else(|| value.as_f64().unwrap_or_default().to_string())
}

fn priority_from_field(github: &GithubConfig, field_values: &[Value]) -> Option<i64> {
    let field_name = github.priority_field_name.as_ref()?;
    let raw = field_value_by_name(field_values, field_name)?;
    raw.trim().parse::<i64>().ok().or_else(|| {
        github
            .priority_labels
            .iter()
            .find_map(|(name, priority)| name.eq_ignore_ascii_case(raw.trim()).then_some(*priority))
    })
}

fn priority_from_labels(labels: &[String], priorities: &BTreeMap<String, i64>) -> Option<i64> {
    labels.iter().find_map(|label| {
        priorities
            .iter()
            .find_map(|(name, priority)| name.eq_ignore_ascii_case(label).then_some(*priority))
    })
}

fn blockers_from_field(github: &GithubConfig, field_values: &[Value]) -> Option<Vec<BlockerRef>> {
    let field_name = github.blocker_field_name.as_ref()?;
    let raw = field_value_by_name(field_values, field_name)?;
    let blockers: Vec<BlockerRef> = raw
        .split([',', '\n', ' '])
        .filter_map(parse_blocker)
        .collect();
    (!blockers.is_empty()).then_some(blockers)
}

fn blockers_from_labels(github: &GithubConfig, labels: &[String]) -> Option<Vec<BlockerRef>> {
    let prefix = github.blocker_label_prefix.as_ref()?.to_ascii_lowercase();
    let blockers: Vec<BlockerRef> = labels
        .iter()
        .filter_map(|label| label.strip_prefix(&prefix))
        .filter_map(parse_blocker)
        .collect();
    (!blockers.is_empty()).then_some(blockers)
}

fn parse_blocker(raw: &str) -> Option<BlockerRef> {
    let value = raw.trim().trim_start_matches(':').trim();
    if value.is_empty() {
        return None;
    }
    let (id, identifier) =
        if value.starts_with("I_") || value.starts_with("MDU") || value.len() > 20 {
            (Some(value.to_string()), None)
        } else {
            (None, Some(value.to_string()))
        };
    Some(BlockerRef {
        id,
        identifier,
        state: None,
    })
}

fn normalized_set(states: &[String]) -> BTreeSet<String> {
    states
        .iter()
        .map(|state| normalize_state_name(state))
        .collect()
}

fn normalize_state_name(state: &str) -> String {
    state.trim().to_ascii_lowercase()
}

fn extend_connection_nodes(
    connection: &mut serde_json::Map<String, Value>,
    nodes: &[Value],
) -> Result<()> {
    let values = connection
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| tracker_error("github_malformed", "missing connection nodes"))?;
    values.extend(nodes.iter().cloned());
    Ok(())
}

fn tracker_error(kind: &'static str, message: impl Into<String>) -> SymphonyError {
    SymphonyError::tracker(kind, message)
}
