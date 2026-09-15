use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal;
use tracing::{error, info, warn};

use crate::channels::{linear, signal as signal_channel, slack, telegram};
use crate::config::Config;
use crate::cron::{CronConfig, CronService, SystemClock};
use crate::memory::MemoryIndex;
use crate::pairing::PairingStore;
use crate::runtime::{Runtime, lock};
use crate::setup;

pub async fn run() -> Result<()> {
    if !Config::exists()? {
        println!("Cica is not configured yet.");
        println!("Run `cica init` to get started.");
        return Ok(());
    }

    let config = Arc::new(Config::load()?);
    let paths = Arc::new(crate::config::paths()?);
    crate::audit::init(paths.audit_db.clone(), config.audit);
    let channels = config.configured_channels();

    if channels.is_empty() {
        println!("No channels configured.");
        println!("Run `cica init` to add a channel.");
        return Ok(());
    }

    let provider: Arc<dyn crate::sandbox::SandboxProvider> = Arc::from(
        crate::sandbox::try_default_provider(&config, &paths)
            .context("invalid [deployment] configuration")?,
    );
    let pairing = PairingStore::load(&paths)?;
    let cron = CronService::new(
        SystemClock,
        CronConfig::default(),
        config.clone(),
        paths.clone(),
        provider.clone(),
    )?;
    let rt = Arc::new(Runtime {
        config,
        paths,
        provider,
        pairing: std::sync::Mutex::new(pairing),
        session_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
        session_ticket: std::sync::atomic::AtomicU64::new(0),
        cron,
    });

    info!("Starting Cica with channels: {}", channels.join(", "));

    info!("Preparing runtime...");
    if let Err(e) = setup::ensure_deps(&rt.config, &rt.paths).await {
        warn!("Failed to prepare dependencies: {}", e);
    }

    index_all_user_memories(&rt.paths, &lock(&rt.pairing));
    rt.cron.start(cron_result_sender(&rt));

    // Skills git-sync (router-side): keep skills_dir + the state store's "skills"
    // prefix fresh from the configured repo. No-op when [skills] is unset.
    if let Some(skills_cfg) = rt.config.skills.clone() {
        let store = match crate::sandbox::state::default_store(&rt.config, &rt.paths) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to build state store for skills sync (continuing local-only): {e}");
                None
            }
        };
        tokio::spawn(crate::skills_sync::run_sync_loop(
            skills_cfg,
            store,
            rt.paths.skills_dir.clone(),
        ));
        info!("Skills sync started");
    }

    let mut handles = Vec::new();

    if let Some(telegram_config) = rt.config.channels.telegram.clone() {
        let rt = rt.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = telegram::run(telegram_config, rt).await {
                error!("Telegram channel error: {}", e);
            }
        }));
    }

    if let Some(signal_config) = rt.config.channels.signal.clone() {
        let rt = rt.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = signal_channel::run(signal_config, rt).await {
                error!("Signal channel error: {}", e);
            }
        }));
    }

    if let Some(slack_config) = rt.config.channels.slack.clone() {
        let rt = rt.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = slack::run(slack_config, rt).await {
                error!("Slack channel error: {}", e);
            }
        }));
    }

    if let Some(linear_config) = rt.config.channels.linear.clone() {
        let rt = rt.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = linear::run(linear_config, rt).await {
                error!("Linear channel error: {}", e);
            }
        }));
    }

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

    rt.cron.stop();

    Ok(())
}

fn cron_result_sender(rt: &Runtime) -> crate::cron::ResultSender {
    let telegram_token = rt
        .config
        .channels
        .telegram
        .as_ref()
        .map(|c| c.bot_token.clone());
    let signal_phone = rt
        .config
        .channels
        .signal
        .as_ref()
        .map(|c| c.phone_number.clone());
    let slack_bot_token = rt
        .config
        .channels
        .slack
        .as_ref()
        .map(|c| c.bot_token.clone());
    let linear_config = rt.config.channels.linear.clone();
    let linear_paths = rt.paths.clone();

    let result_sender: crate::cron::ResultSender =
        Arc::new(move |channel, user_id, target, message| {
            let telegram_token = telegram_token.clone();
            let signal_phone = signal_phone.clone();
            let slack_bot_token = slack_bot_token.clone();
            let linear_config = linear_config.clone();
            let linear_paths = linear_paths.clone();

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
                    "slack" => {
                        if let Some(token) = slack_bot_token {
                            let effective_channel = target.resolve_channel_id(&user_id);
                            send_slack_message(
                                &token,
                                effective_channel,
                                target.thread_id.as_deref(),
                                &message,
                            )
                            .await
                        } else {
                            Err(anyhow::anyhow!("Slack not configured"))
                        }
                    }
                    "linear" => {
                        if let Some(config) = linear_config {
                            // For Linear the "user id" a cron job carries is the
                            // agent session the activity belongs to.
                            crate::channels::linear::send_activity(
                                &config,
                                linear_paths.as_ref(),
                                &user_id,
                                &message,
                            )
                            .await
                        } else {
                            Err(anyhow::anyhow!("Linear not configured"))
                        }
                    }
                    _ => Err(anyhow::anyhow!("Unknown channel: {}", channel)),
                }
            }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
        });

    info!("Cron scheduler started");
    result_sender
}

async fn send_telegram_message(token: &str, user_id: &str, message: &str) -> Result<()> {
    use teloxide::prelude::*;

    let bot = Bot::new(token);
    let chat_id: i64 = user_id.parse()?;
    bot.send_message(ChatId(chat_id), message).await?;
    Ok(())
}

async fn send_signal_message(recipient: &str, message: &str) -> Result<()> {
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::core::params::ObjectParams;
    use jsonrpsee::http_client::HttpClientBuilder;
    use serde_json::Value;

    let url = "http://127.0.0.1:18080/api/v1/rpc";
    let client = HttpClientBuilder::default().build(url)?;

    let mut params = ObjectParams::new();
    params.insert("recipient", vec![recipient])?;
    params.insert("message", message)?;

    let _: Value = client.request("send", params).await?;
    Ok(())
}

async fn send_slack_message(
    bot_token: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
    message: &str,
) -> Result<()> {
    use crate::channels::slack::{markdown_message_chunks, markdown_to_mrkdwn};
    use slack_morphism::prelude::*;

    let client = SlackClient::new(SlackClientHyperConnector::new()?);
    let token = SlackApiToken::new(bot_token.into());
    let session = client.open_session(&token);

    for chunk in markdown_message_chunks(message) {
        let content = SlackMessageContent::new()
            .with_text(markdown_to_mrkdwn(&chunk))
            .with_blocks(vec![SlackMarkdownBlock::new(chunk).into()]);
        let mut request = SlackApiChatPostMessageRequest::new(channel_id.into(), content);

        if let Some(ts) = thread_ts {
            request = request.with_thread_ts(ts.into());
        }

        session.chat_post_message(&request).await?;
    }
    Ok(())
}

/// Startup warm-up: index whatever memories are already on local disk. In cloud
/// mode the per-turn `reindex_user_memories` hook is authoritative (it pulls from
/// the store first), so the index converges after the first turn either way.
fn index_all_user_memories(paths: &crate::config::Paths, store: &PairingStore) {
    let mut index = match MemoryIndex::open(paths) {
        Ok(i) => i,
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
            return;
        }
    };

    for (channel, user_ids) in &store.approved {
        for user_id in user_ids {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
    }

    info!("Memory indexing complete");
}
