//! File-backed workflow repository mutations.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::config::{EffectiveConfig, GithubRepositoryConfig};
use crate::error::{Result, SymphonyError};
use crate::workflow::{load_workflow, parse_workflow};

static TEMP_NAME_START: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryMutation {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryList {
    pub repositories: Vec<GithubRepositoryConfig>,
}

struct EditableWorkflow<'a> {
    config: Mapping,
    body: &'a str,
}

pub fn add_repository(path: &Path, repo: RepositoryMutation) -> Result<RepositoryList> {
    mutate_repositories(path, repo, MutationKind::Add)
}

pub fn remove_repository(path: &Path, repo: RepositoryMutation) -> Result<RepositoryList> {
    mutate_repositories(path, repo, MutationKind::Remove)
}

enum MutationKind {
    Add,
    Remove,
}

fn mutate_repositories(
    path: &Path,
    repo: RepositoryMutation,
    kind: MutationKind,
) -> Result<RepositoryList> {
    load_workflow(path)?;
    let contents =
        fs::read_to_string(path).map_err(|err| SymphonyError::io(path.to_path_buf(), err))?;
    let mut workflow = parse_editable_workflow(path.to_path_buf(), &contents)?;
    let target = normalize_mutation(repo)?;
    let mut repositories = read_repositories(&workflow.config)?;

    match kind {
        MutationKind::Add => {
            if repositories
                .iter()
                .any(|repository| same_repository(repository, &target))
            {
                return Err(SymphonyError::config(
                    "duplicate_repository",
                    "repository is already configured",
                ));
            }
            repositories.push(target);
        }
        MutationKind::Remove => {
            let Some(index) = repositories
                .iter()
                .position(|repository| same_repository(repository, &target))
            else {
                return Err(SymphonyError::config(
                    "repository_not_found",
                    "repository is not configured",
                ));
            };
            if repositories.len() == 1 {
                return Err(SymphonyError::config(
                    "last_repository",
                    "at least one repository must remain configured",
                ));
            }
            repositories.remove(index);
        }
    }

    write_repositories(&mut workflow.config, &repositories)?;
    let rewritten = render_workflow(&workflow.config, workflow.body)?;
    validate_rewritten_workflow(path, &rewritten)?;
    write_atomic(path, rewritten.as_bytes())?;

    Ok(RepositoryList { repositories })
}

fn parse_editable_workflow(path: PathBuf, contents: &str) -> Result<EditableWorkflow<'_>> {
    if contents.starts_with("---") {
        parse_front_matter(path, contents)
    } else {
        Ok(EditableWorkflow {
            config: Mapping::new(),
            body: contents,
        })
    }
}

fn parse_front_matter(path: PathBuf, contents: &str) -> Result<EditableWorkflow<'_>> {
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
    Ok(EditableWorkflow {
        config,
        body: &rest[body_start..],
    })
}

fn normalize_mutation(repo: RepositoryMutation) -> Result<GithubRepositoryConfig> {
    let owner = repo.owner.trim().to_string();
    let name = repo.name.trim().to_string();
    if owner.is_empty() || name.is_empty() {
        return Err(SymphonyError::config(
            "invalid_repository",
            "repository owner and name must be non-empty",
        ));
    }
    Ok(GithubRepositoryConfig { owner, name })
}

fn read_repositories(config: &Mapping) -> Result<Vec<GithubRepositoryConfig>> {
    let Some(tracker) = config.get(key("tracker")) else {
        return Ok(Vec::new());
    };
    let Some(tracker) = tracker.as_mapping() else {
        return Err(SymphonyError::config(
            "invalid_github_repository_config",
            "tracker must be a map",
        ));
    };

    if let Some(value) = tracker.get(key("repositories")) {
        let Some(items) = value.as_sequence() else {
            return Err(SymphonyError::config(
                "invalid_github_repositories",
                "tracker.repositories must be a list",
            ));
        };
        let mut repositories = Vec::with_capacity(items.len());
        for item in items {
            let Some(mapping) = item.as_mapping() else {
                return Err(SymphonyError::config(
                    "invalid_github_repositories",
                    "tracker.repositories entries must be maps",
                ));
            };
            repositories.push(GithubRepositoryConfig {
                owner: mapping_string(mapping, "owner").trim().to_string(),
                name: mapping_string(mapping, "name").trim().to_string(),
            });
        }
        return Ok(repositories);
    }

    let repository = tracker.get(key("repository")).and_then(Value::as_mapping);
    let owner = mapping_string_opt(repository, "owner")
        .or_else(|| mapping_string_opt(Some(tracker), "repository_owner"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let name = mapping_string_opt(repository, "name")
        .or_else(|| mapping_string_opt(Some(tracker), "repository_name"))
        .unwrap_or_default()
        .trim()
        .to_string();

    if owner.is_empty() && name.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![GithubRepositoryConfig { owner, name }])
    }
}

fn write_repositories(config: &mut Mapping, repositories: &[GithubRepositoryConfig]) -> Result<()> {
    let tracker = ensure_tracker_mapping(config)?;
    tracker.remove(key("repository"));
    tracker.remove(key("repository_owner"));
    tracker.remove(key("repository_name"));
    tracker.insert(
        key("repositories"),
        Value::Sequence(
            repositories
                .iter()
                .map(|repository| {
                    let mut mapping = Mapping::new();
                    mapping.insert(key("owner"), Value::String(repository.owner.clone()));
                    mapping.insert(key("name"), Value::String(repository.name.clone()));
                    Value::Mapping(mapping)
                })
                .collect(),
        ),
    );
    Ok(())
}

fn ensure_tracker_mapping(config: &mut Mapping) -> Result<&mut Mapping> {
    let tracker_key = key("tracker");
    if !config.contains_key(&tracker_key) {
        config.insert(tracker_key.clone(), Value::Mapping(Mapping::new()));
    }
    config
        .get_mut(&tracker_key)
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| {
            SymphonyError::config("invalid_github_repository_config", "tracker must be a map")
        })
}

fn render_workflow(config: &Mapping, body: &str) -> Result<String> {
    let mut yaml = serde_yaml::to_string(config)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(format!("---\n{yaml}---\n{body}"))
}

fn validate_rewritten_workflow(path: &Path, contents: &str) -> Result<()> {
    let workflow = parse_workflow(path.to_path_buf(), contents)?;
    let config = EffectiveConfig::from_workflow(workflow)?;
    config.validate_dispatch()
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temp = temp_path(path);
    let result = (|| {
        fs::write(&temp, contents).map_err(|err| SymphonyError::io(temp.clone(), err))?;
        fs::rename(&temp, path).map_err(|err| SymphonyError::io(path.to_path_buf(), err))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temp_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(
        ".symphony-workflow-{}-{}.tmp",
        std::process::id(),
        TEMP_NAME_START.elapsed().as_nanos()
    ))
}

fn same_repository(left: &GithubRepositoryConfig, right: &GithubRepositoryConfig) -> bool {
    left.owner.eq_ignore_ascii_case(&right.owner) && left.name.eq_ignore_ascii_case(&right.name)
}

fn mapping_string(mapping: &Mapping, name: &str) -> String {
    mapping_string_opt(Some(mapping), name).unwrap_or_default()
}

fn mapping_string_opt(mapping: Option<&Mapping>, name: &str) -> Option<String> {
    mapping
        .and_then(|mapping| mapping.get(key(name)))
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}
