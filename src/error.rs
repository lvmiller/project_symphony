use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, SymphonyError>;

#[derive(Debug, Error)]
pub enum SymphonyError {
    #[error("missing_workflow_file path={path}")]
    MissingWorkflowFile { path: PathBuf },

    #[error("workflow_path_not_file path={path}")]
    WorkflowPathNotFile { path: PathBuf },

    #[error("workflow_parse_error path={path} message={message}")]
    WorkflowParseError { path: PathBuf, message: String },

    #[error("workflow_front_matter_not_a_map path={path}")]
    WorkflowFrontMatterNotMap { path: PathBuf },

    #[error("config_validation_error code={code} message={message}")]
    ConfigValidation { code: &'static str, message: String },

    #[error("unsupported_tracker_kind kind={kind}")]
    UnsupportedTrackerKind { kind: String },

    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,

    #[error("missing_github_config field={field}")]
    MissingGithubConfig { field: &'static str },

    #[error("template_parse_error message={0}")]
    TemplateParseError(String),

    #[error("template_render_error message={0}")]
    TemplateRenderError(String),

    #[error("workspace_error message={0}")]
    Workspace(String),

    #[error("hook_error hook={hook} message={message}")]
    Hook { hook: &'static str, message: String },

    #[error("tracker_error kind={kind} message={message}")]
    Tracker { kind: &'static str, message: String },

    #[error("codex_error kind={kind} message={message}")]
    Codex { kind: &'static str, message: String },

    #[error("io_error path={path:?} message={source}")]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: std::io::Error,
    },

    #[error("yaml_error message={0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("json_error message={0}")]
    Json(#[from] serde_json::Error),

    #[error("http_error message={0}")]
    Http(#[from] reqwest::Error),
}

impl SymphonyError {
    pub fn config(code: &'static str, message: impl Into<String>) -> Self {
        Self::ConfigValidation {
            code,
            message: message.into(),
        }
    }

    pub fn tracker(kind: &'static str, message: impl Into<String>) -> Self {
        Self::Tracker {
            kind,
            message: message.into(),
        }
    }

    pub fn codex(kind: &'static str, message: impl Into<String>) -> Self {
        Self::Codex {
            kind,
            message: message.into(),
        }
    }

    pub fn io(path: impl Into<Option<PathBuf>>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
