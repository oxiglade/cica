use anyhow::Result;
use tracing::info;

use crate::channels::telegram;
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

    // For now, we only support Telegram
    // Future: spawn tasks for each channel
    if let Some(telegram_config) = config.channels.telegram {
        telegram::run(telegram_config).await?;
    }

    Ok(())
}
