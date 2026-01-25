use anyhow::Result;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction};
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::{
    CommandResult, handle_onboarding, process_command, query_claude_with_session,
    reindex_user_memories,
};
use crate::config::TelegramConfig;
use crate::onboarding;
use crate::pairing::PairingStore;

/// Start sending periodic typing indicators until cancelled.
/// Returns a sender that, when dropped or sent to, stops the typing loop.
fn start_typing_indicator(bot: Bot, chat_id: ChatId) -> oneshot::Sender<()> {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            // Send typing indicator
            let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

            // Wait 4 seconds or until cancelled (typing indicator lasts ~5s)
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(4)) => {
                    // Continue loop, send typing again
                }
                _ = &mut cancel_rx => {
                    // Cancelled, stop the loop
                    break;
                }
            }
        }
    });

    cancel_tx
}

/// Validate a Telegram bot token by calling getMe
/// Returns the bot username on success
pub async fn validate_token(token: &str) -> Result<String> {
    let bot = Bot::new(token);
    let me = bot.get_me().await?;
    Ok(me.username().to_string())
}

/// Run the Telegram bot
pub async fn run(config: TelegramConfig) -> Result<()> {
    let bot = Bot::new(&config.bot_token);

    info!("Starting Telegram bot...");

    // Register bot commands for the UI menu
    let commands = vec![
        BotCommand::new("new", "Start a new conversation"),
        BotCommand::new("commands", "Show available commands"),
    ];
    if let Err(e) = bot.set_my_commands(commands).await {
        warn!("Failed to set bot commands: {}", e);
    }

    teloxide::repl(bot, move |bot: Bot, msg: Message| async move {
        if let Err(e) = handle_message(&bot, &msg).await {
            warn!("Error handling message: {}", e);
        }
        Ok(())
    })
    .await;

    Ok(())
}

/// Handle an incoming message
async fn handle_message(bot: &Bot, msg: &Message) -> Result<()> {
    let user = msg.from.as_ref();
    let user_id = user.map(|u| u.id.0.to_string()).unwrap_or_default();
    let username = user.and_then(|u| u.username.clone());
    let display_name = user.map(|u| match &u.last_name {
        Some(last) => format!("{} {}", u.first_name, last),
        None => u.first_name.clone(),
    });

    // Check if user is approved
    let mut store = PairingStore::load()?;

    if !store.is_approved("telegram", &user_id) {
        // Create or get existing pairing request
        let (code, _is_new) =
            store.get_or_create_pending("telegram", &user_id, username, display_name)?;

        let response = format!(
            "Hi! I don't recognize you yet.\n\n\
            Pairing code: {}\n\n\
            Ask the owner to run:\n\
            cica approve {}",
            code, code
        );

        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // User is approved - process the message
    let Some(text) = msg.text() else {
        return Ok(());
    };

    info!("Message from {}: {}", user_id, text);

    // Check if onboarding is complete for this user
    if !onboarding::is_complete_for_user("telegram", &user_id)? {
        // /start triggers onboarding greeting, not treated as an answer
        let message = if text == "/start" { "hi" } else { text };

        // Show typing indicator
        let _ = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await;

        let response = handle_onboarding("telegram", &user_id, message).await?;
        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // Ignore /start after onboarding (already set up)
    if text == "/start" {
        return Ok(());
    }

    // Check for commands
    if let CommandResult::Response(response) =
        process_command(&mut store, "telegram", &user_id, text)?
    {
        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // Start periodic typing indicator (will run until we drop the cancel handle)
    let typing_cancel = start_typing_indicator(bot.clone(), msg.chat.id);

    // Query Claude with context (and resume if we have a session)
    let context_prompt = onboarding::build_context_prompt_for_user(
        Some("Telegram"),
        Some("telegram"),
        Some(&user_id),
        Some(text),
    )?;

    let (response, _session_id) =
        query_claude_with_session(&mut store, "telegram", &user_id, text, context_prompt).await?;

    // Stop typing indicator before sending response
    drop(typing_cancel);

    bot.send_message(msg.chat.id, response).await?;

    // Re-index memories in case Claude saved new ones
    reindex_user_memories("telegram", &user_id);

    Ok(())
}
