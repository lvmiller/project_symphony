use liquid::ParserBuilder;
use serde::Serialize;
use serde_json::json;

use crate::config::{DEFAULT_PROMPT, EffectiveConfig, GithubRepositoryConfig};
use crate::domain::Issue;
use crate::error::{Result, SymphonyError};

pub fn render_prompt(template: &str, issue: &Issue, attempt: Option<u32>) -> Result<String> {
    render_prompt_inner(template, issue, attempt, None)
}

pub fn render_prompt_with_source(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
    source: &PromptSourceContext,
) -> Result<String> {
    render_prompt_inner(template, issue, attempt, Some(source))
}

fn render_prompt_inner(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
    source: Option<&PromptSourceContext>,
) -> Result<String> {
    let effective_template = if template.trim().is_empty() {
        DEFAULT_PROMPT
    } else {
        template
    };
    let parser = ParserBuilder::with_stdlib()
        .build()
        .map_err(|err| SymphonyError::TemplateParseError(err.to_string()))?;
    let parsed = parser
        .parse(effective_template)
        .map_err(|err| SymphonyError::TemplateParseError(err.to_string()))?;
    let globals = liquid::to_object(&PromptContext {
        issue,
        attempt,
        source,
    })
    .map_err(|err| SymphonyError::TemplateRenderError(err.to_string()))?;
    parsed
        .render(&globals)
        .map_err(|err| SymphonyError::TemplateRenderError(err.to_string()))
}

pub fn continuation_prompt(attempt: Option<u32>, turn_number: u32, max_turns: u32) -> String {
    let attempt_text = attempt
        .map(|attempt| attempt.to_string())
        .unwrap_or_else(|| "initial".to_string());
    format!(
        "Continue working on the same issue. Do not repeat the original task prompt. attempt={attempt_text} turn={turn_number}/{max_turns}"
    )
}

pub fn issue_template_value(issue: &Issue, attempt: Option<u32>) -> serde_json::Value {
    json!({ "issue": issue, "attempt": attempt })
}

pub fn issue_template_value_with_source(
    issue: &Issue,
    attempt: Option<u32>,
    source: &PromptSourceContext,
) -> serde_json::Value {
    json!({ "issue": issue, "attempt": attempt, "source": source })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptSourceContext {
    pub id: String,
    pub workflow_path: String,
    pub repository_owner: Option<String>,
    pub repository_name: Option<String>,
    pub repositories: Vec<GithubRepositoryConfig>,
    pub project_owner_login: Option<String>,
    pub project_number: Option<i64>,
}

impl PromptSourceContext {
    pub fn from_config(config: &EffectiveConfig) -> Self {
        let github = config.tracker.github.as_ref();
        Self {
            id: config.source.id.clone(),
            workflow_path: config.workflow_path.display().to_string(),
            repository_owner: github.map(|github| github.repository_owner.clone()),
            repository_name: github.map(|github| github.repository_name.clone()),
            repositories: github
                .map(|github| github.repositories.clone())
                .unwrap_or_default(),
            project_owner_login: github.map(|github| github.project_owner_login.clone()),
            project_number: github.map(|github| github.project_number),
        }
    }
}

#[derive(Serialize)]
struct PromptContext<'a> {
    issue: &'a Issue,
    attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a PromptSourceContext>,
}
