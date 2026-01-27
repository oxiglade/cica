use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction, PhotoSize};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::{
    Channel, TypingGuard, UserTaskManager, build_text_with_images, determine_action,
    execute_action, execute_claude_query,
};
use crate::config::{self, TelegramConfig};
use crate::pairing::PairingStore;

// ============================================================================
// Channel Implementation
// ============================================================================

/// Telegram channel implementation
pub struct TelegramChannel {
    bot: Bot,
    chat_id: ChatId,
}

impl TelegramChannel {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self { bot, chat_id }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn display_name(&self) -> &'static str {
        "Telegram"
    }

    async fn send_message(&self, message: &str) -> Result<()> {
        self.bot.send_message(self.chat_id, message).await?;
        Ok(())
    }

    fn start_typing(&self) -> TypingGuard {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let bot = self.bot.clone();
        let chat_id = self.chat_id;

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

        TypingGuard::new(cancel_tx)
    }
}

// ============================================================================
// Photo Handling
// ============================================================================

/// Get the directory where Telegram attachments are stored
fn get_telegram_attachments_dir() -> Result<PathBuf> {
    let paths = config::paths()?;
    let dir = paths.base.join("telegram_attachments");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Download a photo from Telegram and save it locally
/// Returns the local file path on success
async fn download_photo(bot: &Bot, photo: &PhotoSize) -> Result<PathBuf> {
    let file = bot.get_file(&photo.file.id).await?;
    let file_path = file.path;

    // Determine extension from the file path
    let extension = file_path.rsplit('.').next().unwrap_or("jpg");

    let attachments_dir = get_telegram_attachments_dir()?;
    let local_path = attachments_dir.join(format!("{}.{}", photo.file.unique_id, extension));

    // Skip download if file already exists
    if local_path.exists() {
        debug!("Photo already downloaded: {:?}", local_path);
        return Ok(local_path);
    }

    // Download the file
    let mut dst = tokio::fs::File::create(&local_path).await?;
    bot.download_file(&file_path, &mut dst).await?;

    info!("Downloaded photo to {:?}", local_path);
    Ok(local_path)
}

/// Get the largest photo from a list of photo sizes
fn get_largest_photo(photos: &[PhotoSize]) -> Option<&PhotoSize> {
    photos.iter().max_by_key(|p| p.width * p.height)
}

// ============================================================================
// Public API
// ============================================================================

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

// ============================================================================
// Message Handling
// ============================================================================

/// Handle an incoming message
async fn handle_message(
    bot: &Bot,
    msg: &Message,
    task_manager: Arc<UserTaskManager>,
) -> Result<()> {
    // Extract user info
    let user = msg.from.as_ref();
    let user_id = user.map(|u| u.id.0.to_string()).unwrap_or_default();
    let username = user.and_then(|u| u.username.clone());
    let display_name = user.map(|u| match &u.last_name {
        Some(last) => format!("{} {}", u.first_name, last),
        None => u.first_name.clone(),
    });

    // Get text (either from text message or photo caption)
    let text = msg.text().or(msg.caption()).unwrap_or_default();

    // Download any photos in the message
    let mut image_paths: Vec<PathBuf> = Vec::new();
    if let Some(photos) = msg.photo()
        && let Some(largest) = get_largest_photo(photos)
    {
        match download_photo(bot, largest).await {
            Ok(path) => image_paths.push(path),
            Err(e) => warn!("Failed to download photo: {}", e),
        }
    }

    // Skip if no text and no images
    if text.is_empty() && image_paths.is_empty() {
        return Ok(());
    }

    info!("Message from {}: {}", user_id, text);
    if !image_paths.is_empty() {
        info!(
            "Message includes {} image(s): {:?}",
            image_paths.len(),
            image_paths
        );
    }

    // Create channel wrapper
    let channel: Arc<dyn Channel> = Arc::new(TelegramChannel::new(bot.clone(), msg.chat.id));

    // Determine what action to take
    let mut store = PairingStore::load()?;
    let action = determine_action(
        channel.name(),
        &user_id,
        text,
        &image_paths,
        &mut store,
        username,
        display_name,
    )?;

    // Execute the action
    if let Some(query_text) = execute_action(channel.as_ref(), &user_id, action).await? {
        // QueryClaude action - queue with task manager for debouncing
        let text_with_images = build_text_with_images(&query_text, &image_paths);
        let user_key = format!("{}:{}", channel.name(), user_id);
        let channel_clone = channel.clone();
        let user_id_clone = user_id.clone();

        task_manager
            .process_message(user_key, text_with_images, move |messages| async move {
                execute_claude_query(channel_clone, &user_id_clone, messages).await;
            })
            .await;
    }

    Ok(())
}
