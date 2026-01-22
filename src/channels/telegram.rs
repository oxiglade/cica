use anyhow::Result;
use teloxide::prelude::*;
use tracing::{info, warn};

use crate::claude;
use crate::config::TelegramConfig;
use crate::onboarding;
use crate::pairing::PairingStore;

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
        let (code, is_new) =
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

    // Check if onboarding is complete
    if !onboarding::is_complete()? {
        // /start triggers onboarding greeting, not treated as an answer
        let message = if text == "/start" { "hi" } else { text };
        let response = handle_onboarding(message).await?;
        bot.send_message(msg.chat.id, response).await?;
        return Ok(());
    }

    // Ignore /start after onboarding (already set up)
    if text == "/start" {
        return Ok(());
    }

    // Query Claude
    let response = match claude::query(text).await {
        Ok(response) => response,
        Err(e) => {
            warn!("Claude error: {}", e);
            format!("Sorry, I encountered an error: {}", e)
        }
    };

    bot.send_message(msg.chat.id, response).await?;

    Ok(())
}

/// Handle onboarding flow - Claude drives the conversation
async fn handle_onboarding(message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt()?;

    let options = claude::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true, // Allow writing IDENTITY.md
        ..Default::default()
    };

    let (response, _) = claude::query_with_options(message, options).await?;
    Ok(response)
}
