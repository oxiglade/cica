use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio::signal;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::channels::{signal as signal_channel, telegram};
use crate::config::Config;
use crate::cron::{CronConfig, CronService, SystemClock};
use crate::memory::MemoryIndex;
use crate::pairing::PairingStore;
use crate::setup;

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

    // Ensure runtime dependencies are ready
    info!("Preparing runtime...");
    if let Err(e) = setup::ensure_embedding_model() {
        warn!("Failed to prepare embedding model: {}", e);
    }

    // Index memories for all approved users at startup
    index_all_user_memories();

    // Start cron scheduler service
    let cron_service = start_cron_service(&config)?;

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

    // Stop cron service
    if let Some(service) = cron_service {
        let mut service = service.lock().await;
        service.stop().await;
    }

    Ok(())
}

/// Start the cron scheduler service
fn start_cron_service(config: &Config) -> Result<Option<Arc<Mutex<CronService<SystemClock>>>>> {
    let clock = SystemClock;
    let cron_config = CronConfig::default();

    let mut service = match CronService::new(clock, cron_config) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to initialize cron service: {}", e);
            return Ok(None);
        }
    };

    // Create result sender that routes messages to the appropriate channel
    let telegram_token = config
        .channels
        .telegram
        .as_ref()
        .map(|c| c.bot_token.clone());
    let signal_phone = config
        .channels
        .signal
        .as_ref()
        .map(|c| c.phone_number.clone());

    let result_sender: crate::cron::ResultSender = Arc::new(move |channel, user_id, message| {
        let telegram_token = telegram_token.clone();
        let signal_phone = signal_phone.clone();

        Box::pin(async move {
            match channel.as_str() {
                "telegram" => {
                    if let Some(token) = telegram_token {
                        send_telegram_message(&token, &user_id, &message).await
                    } else {
                        Err(anyhow::anyhow!("Telegram not configured"))
                    }
                }
                "signal" => {
                    if let Some(_phone) = signal_phone {
                        send_signal_message(&user_id, &message).await
                    } else {
                        Err(anyhow::anyhow!("Signal not configured"))
                    }
                }
                _ => Err(anyhow::anyhow!("Unknown channel: {}", channel)),
            }
        }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
    });

    service.start(result_sender);
    info!("Cron scheduler started");

    Ok(Some(Arc::new(Mutex::new(service))))
}

/// Send a message via Telegram
async fn send_telegram_message(token: &str, user_id: &str, message: &str) -> Result<()> {
    use teloxide::prelude::*;

    let bot = Bot::new(token);
    let chat_id: i64 = user_id.parse()?;
    bot.send_message(ChatId(chat_id), message).await?;
    Ok(())
}

/// Send a message via Signal
async fn send_signal_message(recipient: &str, message: &str) -> Result<()> {
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::core::params::ObjectParams;
    use jsonrpsee::http_client::HttpClientBuilder;
    use serde_json::Value;

    // Connect to the signal-cli daemon
    let url = "http://127.0.0.1:18080/api/v1/rpc";
    let client = HttpClientBuilder::default().build(url)?;

    let mut params = ObjectParams::new();
    params.insert("recipient", vec![recipient])?;
    params.insert("message", message)?;

    let _: Value = client.request("send", params).await?;
    Ok(())
}

/// Index memories for all approved users
fn index_all_user_memories() {
    let store = match PairingStore::load() {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to load pairing store for memory indexing: {}", e);
            return;
        }
    };

    let mut index = match MemoryIndex::open() {
        Ok(i) => i,
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
            return;
        }
    };

    // Index memories for each approved user
    for key in store.approved.keys() {
        // Key format is "channel:user_id"
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (channel, user_id) = (parts[0], parts[1]);

        if let Err(e) = index.index_user_memories(channel, user_id) {
            warn!(
                "Failed to index memories for {}:{}: {}",
                channel, user_id, e
            );
        }
    }

    info!("Memory indexing complete");
}
