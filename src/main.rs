use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use symphony::config::{
    ConfigSetReloader, EffectiveConfig, TrackerApiKeySource, WorkflowDiagnostics,
    WorkspaceRootSource, workflow_diagnostics,
};
use symphony::logging::init_logging;
use symphony::service::run_multi_source_service_until_shutdown;
use symphony::shutdown::shutdown_signal;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "symphony",
    version,
    about = "Symphony coding-agent orchestrator"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(value_name = "WORKFLOW.md")]
    workflows: Vec<PathBuf>,

    #[arg(long, value_name = "HOST")]
    host: Option<IpAddr>,

    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    #[arg(long, hide = true)]
    check: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and validate workflow configuration without starting Symphony.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate a workflow's dispatch configuration.
    Validate {
        #[arg(value_name = "WORKFLOW.md")]
        workflow: Option<PathBuf>,
    },
    /// Explain a workflow's effective configuration without exposing secrets.
    Explain {
        #[arg(value_name = "WORKFLOW.md")]
        workflow: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = ExplainFormat::Text)]
        format: ExplainFormat,
    },
    /// Diagnose workflow configuration prerequisites without exposing secrets.
    Doctor {
        #[arg(value_name = "WORKFLOW.md")]
        workflow: Option<PathBuf>,
    },
    /// Emit the machine-readable workflow configuration schema.
    Schema {
        #[arg(long, value_enum, default_value_t = SchemaFormat::JsonSchema)]
        format: SchemaFormat,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExplainFormat {
    Json,
    Text,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SchemaFormat {
    JsonSchema,
}

#[tokio::main]
async fn main() {
    let Cli {
        command,
        workflows,
        host,
        port,
        check,
    } = Cli::parse();

    if let Some(command) = command {
        if let Err(error) = run_config_command(command) {
            eprintln!("config_failed error=\"{error}\"");
            std::process::exit(1);
        }
        return;
    }

    init_logging();
    let workflow_paths = if workflows.is_empty() {
        vec![PathBuf::from("WORKFLOW.md")]
    } else {
        workflows
    };
    match ConfigSetReloader::new(workflow_paths) {
        Ok(reloader) => {
            let sources: Vec<_> = reloader
                .current()
                .map(|config| format!("{}:{}", config.source.id, config.workflow_path.display()))
                .collect();
            info!(sources = %sources.join(","), "startup completed");
            if check {
                info!("check completed");
                return;
            }
            let server_bind = match reloader.initial_server_bind(host, port) {
                Ok(server_bind) => server_bind,
                Err(error) => {
                    eprintln!("startup_failed error=\"{error}\"");
                    std::process::exit(1);
                }
            };
            if let Err(error) =
                run_multi_source_service_until_shutdown(reloader, shutdown_signal(), server_bind)
                    .await
            {
                eprintln!("host_error error=\"{error}\"");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("startup_failed error=\"{error}\"");
            std::process::exit(1);
        }
    }
}

fn run_config_command(command: Command) -> symphony::Result<()> {
    match command {
        Command::Config {
            command: ConfigCommand::Validate { workflow },
        } => {
            let config = EffectiveConfig::load(workflow)?;
            config.validate_dispatch()?;
            println!("configuration is valid");
        }
        Command::Config {
            command: ConfigCommand::Explain { workflow, format },
        } => {
            let config = EffectiveConfig::load(workflow)?;
            let report = effective_config_report(&config)?;
            match format {
                ExplainFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
                ExplainFormat::Text => print!("{}", serde_yaml::to_string(&report)?),
            }
        }
        Command::Config {
            command: ConfigCommand::Doctor { workflow },
        } => {
            let diagnostics = workflow_diagnostics(workflow)?;
            print!("{}", serde_yaml::to_string(&doctor_report(&diagnostics)?)?);
            if !diagnostics.is_healthy() {
                return Err(symphony::SymphonyError::config(
                    "config_doctor_failed",
                    "workflow diagnostics found configuration prerequisites that must be fixed",
                ));
            }
        }
        Command::Config {
            command: ConfigCommand::Schema { format },
        } => match format {
            SchemaFormat::JsonSchema => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&symphony::config::raw_workflow_json_schema())?
                );
            }
        },
    }
    Ok(())
}

fn effective_config_report(config: &EffectiveConfig) -> symphony::Result<Value> {
    let api_key_present = config
        .tracker
        .api_key
        .as_deref()
        .is_some_and(|api_key| !api_key.is_empty());
    let mut report = serde_json::to_value(config)?;
    let Some(tracker) = report.get_mut("tracker").and_then(Value::as_object_mut) else {
        return Err(symphony::SymphonyError::config(
            "invalid_effective_config_report",
            "effective configuration did not serialize a tracker object",
        ));
    };
    tracker.insert("api_key_present".to_string(), Value::Bool(api_key_present));
    Ok(report)
}

fn doctor_report(diagnostics: &WorkflowDiagnostics) -> symphony::Result<Value> {
    let tracker_api_key_source = match &diagnostics.tracker_api_key.source {
        TrackerApiKeySource::Literal => "literal".to_string(),
        TrackerApiKeySource::Environment { variable } => format!("${variable}"),
        TrackerApiKeySource::Missing => "missing".to_string(),
    };
    let (workspace_root_source, workspace_root_environment) =
        match &diagnostics.workspace_root.source {
            WorkspaceRootSource::Literal => ("literal".to_string(), None),
            WorkspaceRootSource::Environment { variable } => {
                (format!("${variable}"), Some(variable.as_str()))
            }
            WorkspaceRootSource::Default => ("default".to_string(), None),
        };

    Ok(serde_json::json!({
        "workflow_path": diagnostics.workflow_path,
        "parse_status": diagnostics.parse,
        "dispatch_validation_status": diagnostics.dispatch,
        "tracker_api_key": {
            "source": tracker_api_key_source,
            "presence": diagnostics.tracker_api_key.presence,
        },
        "workspace_root": {
            "source": workspace_root_source,
            "environment": workspace_root_environment,
            "presence": diagnostics.workspace_root.environment_presence,
            "status": diagnostics.workspace_root.status,
            "normalized_path": diagnostics.workspace_root.normalized_path,
        },
    }))
}
