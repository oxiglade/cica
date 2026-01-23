use anyhow::Result;
use tokio::signal;
use tracing::{error, info};

use crate::channels::{signal as signal_channel, telegram};
use crate::config::Config;

/// Run the assistant (default command)
pub async fn run() -> Result<()> {
    // Check if configured
    if !Config::exists()? {
        println!("Cica is not configured yet.");
        println!("Run `cica init` to get started.");
        return Ok(());
    }

    let config = Config::load()?;
    let channels = config.configured_channels();

    if channels.is_empty() {
        println!("No channels configured.");
        println!("Run `cica init` to add a channel.");
        return Ok(());
    }

    info!("Starting Cica with channels: {}", channels.join(", "));

    // Spawn tasks for each configured channel
    let mut handles = Vec::new();

    if let Some(telegram_config) = config.channels.telegram {
        handles.push(tokio::spawn(async move {
            if let Err(e) = telegram::run(telegram_config).await {
                error!("Telegram channel error: {}", e);
            }
        }));
    }

    if let Some(signal_config) = config.channels.signal {
        handles.push(tokio::spawn(async move {
            if let Err(e) = signal_channel::run(signal_config).await {
                error!("Signal channel error: {}", e);
            }
        }));
    }

    // Wait for Ctrl+C
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = async {
            for handle in handles {
                let _ = handle.await;
            }
        } => {}
    }

    Ok(())
}
