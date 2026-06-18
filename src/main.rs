use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;
use symphony::config::ConfigSetReloader;
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
    #[arg(value_name = "WORKFLOW.md")]
    workflows: Vec<PathBuf>,

    #[arg(long, value_name = "HOST")]
    host: Option<IpAddr>,

    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    #[arg(long, hide = true)]
    check: bool,
}

#[tokio::main]
async fn main() {
    init_logging();
    let cli = Cli::parse();
    let workflow_paths = if cli.workflows.is_empty() {
        vec![PathBuf::from("WORKFLOW.md")]
    } else {
        cli.workflows
    };
    match ConfigSetReloader::new(workflow_paths) {
        Ok(reloader) => {
            let sources: Vec<_> = reloader
                .current()
                .map(|config| format!("{}:{}", config.source.id, config.workflow_path.display()))
                .collect();
            info!(sources = %sources.join(","), "startup completed");
            if cli.check {
                info!("check completed");
                return;
            }
            let server_bind = match reloader.initial_server_bind(cli.host, cli.port) {
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
