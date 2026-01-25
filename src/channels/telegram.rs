use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction};
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::{
    CommandResult, UserTaskManager, handle_onboarding, process_command, query_claude_with_session,
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
        BotCommand::new("skills", "List available skills"),
        BotCommand::new("commands", "Show available commands"),
    ];
    if let Err(e) = bot.set_my_commands(commands).await {
        warn!("Failed to set bot commands: {}", e);
    }

    // Create shared task manager for per-user message handling
    let task_manager = UserTaskManager::new();

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let task_manager = Arc::clone(&task_manager);
        async move {
            if let Err(e) = handle_message(&bot, &msg, task_manager).await {
                warn!("Error handling message: {}", e);
            }
            Ok(())
        }
    })
    .await;

    Ok(())
}

/// Handle an incoming message
async fn handle_message(
    bot: &Bot,
    msg: &Message,
    task_manager: Arc<UserTaskManager>,
) -> Result<()> {
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
    let onboarding_complete = onboarding::is_complete_for_user("telegram", &user_id)?;

    // Check for commands first (works even during onboarding)
    let cmd_result = process_command(&mut store, "telegram", &user_id, text, onboarding_complete)?;
    if let CommandResult::Response(response) = cmd_result {
        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    if !onboarding_complete {
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

    // Queue the message for processing with debounce and interruption support
    let user_key = format!("telegram:{}", user_id);
    let bot_clone = bot.clone();
    let chat_id = msg.chat.id;
    let user_id_clone = user_id.clone();
    let text_owned = text.to_string();

    task_manager
        .process_message(user_key, text_owned, move |messages| async move {
            // Combine multiple messages if batched
            let combined_text = messages.join("\n\n");

            // Start periodic typing indicator
            let typing_cancel = start_typing_indicator(bot_clone.clone(), chat_id);

            // Query Claude with context
            let context_prompt = match onboarding::build_context_prompt_for_user(
                Some("Telegram"),
                Some("telegram"),
                Some(&user_id_clone),
                Some(&combined_text),
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to build context prompt: {}", e);
                    drop(typing_cancel);
                    let _ = bot_clone
                        .send_message(chat_id, format!("Sorry, I encountered an error: {}", e))
                        .await;
                    return;
                }
            };

            let mut store = match PairingStore::load() {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to load pairing store: {}", e);
                    drop(typing_cancel);
                    let _ = bot_clone
                        .send_message(chat_id, format!("Sorry, I encountered an error: {}", e))
                        .await;
                    return;
                }
            };

            let (response, _session_id) = match query_claude_with_session(
                &mut store,
                "telegram",
                &user_id_clone,
                &combined_text,
                context_prompt,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("Claude query failed: {}", e);
                    drop(typing_cancel);
                    let _ = bot_clone
                        .send_message(chat_id, format!("Sorry, I encountered an error: {}", e))
                        .await;
                    return;
                }
            };

            // Stop typing indicator before sending response
            drop(typing_cancel);

            if let Err(e) = bot_clone.send_message(chat_id, response).await {
                warn!("Failed to send message: {}", e);
            }

            // Re-index memories in case Claude saved new ones
            reindex_user_memories("telegram", &user_id_clone);
        })
        .await;

    Ok(())
}
