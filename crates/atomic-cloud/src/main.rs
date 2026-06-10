//! Atomic Cloud — binary entry point.
//!
//! Today this exposes control-plane bootstrap: `atomic-cloud migrate`
//! connects to the control-plane database (creating it on first boot) and
//! applies pending migrations. The composed multi-tenant server (`serve`)
//! arrives with the tenant-routing slice; subcommands keep that addition
//! purely additive.

use atomic_cloud::ControlPlane;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "atomic-cloud", about = "Atomic Cloud multi-tenant server")]
struct Cli {
    /// Postgres URL of the control-plane database. When the URL omits a
    /// database name, `atomic_cloud_control` is used.
    #[arg(long, env = "ATOMIC_CLOUD_CONTROL_URL")]
    control_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the control plane (creating the database if it doesn't
    /// exist) and apply pending migrations.
    Migrate,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atomic_cloud=info,warn".parse().unwrap());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();

    match cli.command {
        Command::Migrate => {
            let control = match ControlPlane::connect(&cli.control_url).await {
                Ok(control) => control,
                Err(e) => {
                    tracing::error!("control-plane connection failed: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match control.initialize().await {
                Ok(applied) => {
                    tracing::info!(applied, "control-plane migrations complete");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    tracing::error!("control-plane migration failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}
