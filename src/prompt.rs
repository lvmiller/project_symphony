use liquid::ParserBuilder;
use serde::Serialize;
use serde_json::json;

use crate::config::DEFAULT_PROMPT;
use crate::domain::Issue;
use crate::error::{Result, SymphonyError};

pub fn render_prompt(template: &str, issue: &Issue, attempt: Option<u32>) -> Result<String> {
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
    let globals = liquid::to_object(&PromptContext { issue, attempt })
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

#[derive(Serialize)]
struct PromptContext<'a> {
    issue: &'a Issue,
    attempt: Option<u32>,
}
