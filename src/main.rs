use std::path::PathBuf;

use clap::Parser;
use symphony::config::ConfigReloader;
use symphony::logging::init_logging;
use symphony::service::run_service_until_shutdown;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "symphony",
    version,
    about = "Symphony coding-agent orchestrator"
)]
struct Cli {
    #[arg(value_name = "WORKFLOW.md", default_value = "WORKFLOW.md")]
    workflow: PathBuf,

    #[arg(long, hide = true)]
    check: bool,
}

#[tokio::main]
async fn main() {
    init_logging();
    let cli = Cli::parse();
    match ConfigReloader::new(Some(cli.workflow)) {
        Ok(reloader) => {
            info!(workflow_path = %reloader.current().workflow_path.display(), "startup completed");
            if cli.check {
                info!("check completed");
                return;
            }
            if let Err(error) = run_service_until_shutdown(reloader, shutdown_signal()).await {
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

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("signal_error error=\"{error}\"");
    }
}
