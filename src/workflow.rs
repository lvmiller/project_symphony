use std::env;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::domain::WorkflowDefinition;
use crate::error::{Result, SymphonyError};

pub fn select_workflow_path(explicit_path: Option<PathBuf>) -> Result<PathBuf> {
    let path = explicit_path.unwrap_or_else(|| PathBuf::from("WORKFLOW.md"));
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(|err| SymphonyError::io(None, err))?
            .join(path)
    };
    Ok(path)
}

pub fn load_workflow(path: &Path) -> Result<WorkflowDefinition> {
    let contents = std::fs::read_to_string(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => SymphonyError::MissingWorkflowFile {
            path: path.to_path_buf(),
        },
        _ => SymphonyError::io(Some(path.to_path_buf()), err),
    })?;
    parse_workflow(path.to_path_buf(), &contents)
}

pub fn parse_workflow(path: PathBuf, contents: &str) -> Result<WorkflowDefinition> {
    let (config, body) = if contents.starts_with("---") {
        parse_front_matter(path.clone(), contents)?
    } else {
        (Mapping::new(), contents)
    };
    Ok(WorkflowDefinition {
        config,
        prompt_template: body.trim().to_string(),
        path,
    })
}

fn parse_front_matter(path: PathBuf, contents: &str) -> Result<(Mapping, &str)> {
    let mut offset = 3;
    if contents.as_bytes().get(3) == Some(&b'\r') && contents.as_bytes().get(4) == Some(&b'\n') {
        offset = 5;
    } else if contents.as_bytes().get(3) == Some(&b'\n') {
        offset = 4;
    }
    let rest = &contents[offset..];
    let mut yaml_end_in_rest = None;
    let mut body_start_in_rest = None;
    let mut cursor = 0;
    for line in rest.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\r', '\n']);
        if stripped == "---" {
            yaml_end_in_rest = Some(cursor);
            body_start_in_rest = Some(cursor + line.len());
            break;
        }
        cursor += line.len();
    }
    let (yaml_end, body_start) = match (yaml_end_in_rest, body_start_in_rest) {
        (Some(yaml_end), Some(body_start)) => (yaml_end, body_start),
        _ => {
            return Err(SymphonyError::WorkflowParseError {
                path,
                message: "front matter opening delimiter has no closing delimiter".to_string(),
            });
        }
    };
    let yaml = &rest[..yaml_end];
    let value: Value =
        serde_yaml::from_str(yaml).map_err(|err| SymphonyError::WorkflowParseError {
            path: path.clone(),
            message: err.to_string(),
        })?;
    let config = match value {
        Value::Null => Mapping::new(),
        Value::Mapping(mapping) => mapping,
        _ => return Err(SymphonyError::WorkflowFrontMatterNotMap { path }),
    };
    Ok((config, &rest[body_start..]))
}
