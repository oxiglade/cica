mod channels;
mod claude;
mod cmd;
mod config;
mod cron;
mod memory;
mod onboarding;
mod pairing;
mod setup;
mod skills;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cica")]
#[command(about = "A personal AI assistant that lives in your chat")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Set up Cica or add a new channel
    Init,

    /// Approve a pairing request
    Approve {
        /// The pairing code shown to the user
        code: String,
    },

    /// Show where Cica stores its data
    Paths,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Init) => cmd::init::run().await,
        Some(Commands::Approve { code }) => cmd::approve::run(&code),
        Some(Commands::Paths) => cmd::paths::run(),
        None => cmd::run::run().await,
    }
}
