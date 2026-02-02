use anyhow::Result;
use async_trait::async_trait;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::{
    Channel, TypingGuard, UserTaskManager, build_text_with_images, determine_action,
    execute_action, execute_claude_query,
};
use crate::config::{self, SlackConfig};
use crate::pairing::PairingStore;
use crate::skills;

// ============================================================================
// File/Image Handling
// ============================================================================

/// Get the directory where Slack attachments are stored
fn get_slack_attachments_dir() -> Result<PathBuf> {
    let paths = config::paths()?;
    let dir = paths.internal_dir.join("slack_attachments");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Download a file from Slack and save it locally
/// Requires the bot token for authentication
async fn download_slack_file(file: &SlackFile, bot_token: &str) -> Result<PathBuf> {
    let url = file
        .url_private_download
        .as_ref()
        .or(file.url_private.as_ref())
        .ok_or_else(|| anyhow::anyhow!("No download URL for file"))?;

    let file_name = file.name.as_deref().unwrap_or("unknown");
    let file_id = &file.id;

    let attachments_dir = get_slack_attachments_dir()?;
    let local_path = attachments_dir.join(format!("{}_{}", file_id, file_name));

    // Skip download if file already exists
    if local_path.exists() {
        debug!("File already downloaded: {:?}", local_path);
        return Ok(local_path);
    }

    // Download with authorization header
    let client = reqwest::Client::new();
    let response = client
        .get(url.as_str())
        .header("Authorization", format!("Bearer {}", bot_token))
        .send()
        .await?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to download file: {}", response.status());
    }

    let bytes = response.bytes().await?;
    std::fs::write(&local_path, &bytes)?;

    info!("Downloaded Slack file to {:?}", local_path);
    Ok(local_path)
}

/// Check if a file is an image based on mimetype
fn is_image_file(file: &SlackFile) -> bool {
    file.mimetype
        .as_ref()
        .map(|m| m.to_string().starts_with("image/"))
        .unwrap_or(false)
}

/// Set suggested prompts for a new thread based on available skills
async fn set_suggested_prompts(
    client: &Arc<SlackHyperClient>,
    token: &SlackApiToken,
    channel_id: &SlackChannelId,
    thread_ts: &SlackTs,
) {
    let session = client.open_session(token);

    // Build prompts from available skills (up to 4, Slack's limit)
    let mut prompts = Vec::new();

    // Add a default "What can you do?" prompt
    prompts.push(SlackAssistantPrompt::new(
        "What can you help me with?".to_string(),
        "What can you help me with?".to_string(),
    ));

    // Add skills as prompts using their descriptions
    if let Ok(available_skills) = skills::discover_skills() {
        for skill in available_skills.iter().take(3) {
            // Leave room for default prompt
            prompts.push(SlackAssistantPrompt::new(
                skill.description.clone(), // e.g., "Get latest assessment statistics"
                skill.description.clone(), // Send the description as the message
            ));
        }
    }

    let request = SlackApiAssistantThreadsSetSuggestedPromptsRequest::new(
        channel_id.clone(),
        thread_ts.clone(),
        prompts,
    );

    if let Err(e) = session
        .assistant_threads_set_suggested_prompts(&request)
        .await
    {
        warn!("Failed to set suggested prompts: {}", e);
    }
}

// ============================================================================
// Markdown to Slack mrkdwn conversion
// ============================================================================

/// Convert standard Markdown to Slack's mrkdwn format
fn markdown_to_mrkdwn(text: &str) -> String {
    let mut result = text.to_string();

    // Convert bold: **text** -> *text*
    // Be careful not to convert already-correct single asterisks
    // Use a simple approach: replace ** with a placeholder, then convert
    result = result.replace("**", "\x00BOLD\x00");
    result = result.replace("\x00BOLD\x00", "*");

    // Convert italic: *text* -> _text_ (but only single asterisks not part of bold)
    // This is tricky because * is used for bold in mrkdwn
    // Skip this for now as it can conflict with bullet points

    // Convert links: [text](url) -> <url|text>
    let link_re = regex_lite::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = link_re.replace_all(&result, "<$2|$1>").to_string();

    // Convert inline code: `code` stays the same in Slack
    // Convert code blocks: ```code``` stays the same in Slack

    result
}

// ============================================================================
// Channel Implementation
// ============================================================================

/// Slack channel implementation for AI Assistant threads
pub struct SlackChannel {
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    /// The DM channel ID
    channel_id: SlackChannelId,
    /// Thread timestamp - required for AI Assistant apps to reply in the correct thread
    thread_ts: Option<SlackTs>,
}

impl SlackChannel {
    pub fn new(
        client: Arc<SlackHyperClient>,
        token: SlackApiToken,
        channel_id: SlackChannelId,
        thread_ts: Option<SlackTs>,
    ) -> Self {
        Self {
            client,
            token,
            channel_id,
            thread_ts,
        }
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn display_name(&self) -> &'static str {
        "Slack"
    }

    async fn send_message(&self, message: &str) -> Result<()> {
        info!(
            "Sending message to channel {} (thread: {:?})",
            self.channel_id, self.thread_ts
        );
        let session = self.client.open_session(&self.token);

        // Convert markdown to Slack's mrkdwn format
        let mrkdwn_message = markdown_to_mrkdwn(message);

        // Build request with thread_ts if available (required for AI Assistant apps)
        let mut request = SlackApiChatPostMessageRequest::new(
            self.channel_id.clone(),
            SlackMessageContent::new().with_text(mrkdwn_message),
        );

        // Reply in the thread if we have a thread_ts
        if let Some(ts) = &self.thread_ts {
            request = request.with_thread_ts(ts.clone());
        }

        debug!("Request: {:?}", request);

        match session.chat_post_message(&request).await {
            Ok(response) => {
                info!("Message sent successfully, ts: {:?}", response.ts);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send message: {}", e);
                Err(e.into())
            }
        }
    }

    fn start_typing(&self) -> TypingGuard {
        // For Slack AI assistants, we use assistant.threads.setStatus
        // to show a "thinking" indicator
        if let Some(thread_ts) = &self.thread_ts {
            let client = self.client.clone();
            let token = self.token.clone();
            let channel_id = self.channel_id.clone();
            let thread_ts = thread_ts.clone();

            // Set the status to show we're working
            let client_clone = client.clone();
            let token_clone = token.clone();
            let channel_id_clone = channel_id.clone();
            let thread_ts_clone = thread_ts.clone();

            tokio::spawn(async move {
                let session = client_clone.open_session(&token_clone);
                let request = SlackApiAssistantThreadsSetStatusRequest::new(
                    channel_id_clone,
                    "is thinking...".to_string(),
                    thread_ts_clone,
                );
                if let Err(e) = session.assistant_threads_set_status(&request).await {
                    warn!("Failed to set assistant status: {}", e);
                }
            });

            // Return a guard that will clear the status when dropped
            // We use a custom approach since TypingGuard expects a oneshot channel
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

            // Spawn a task that clears status when cancelled
            tokio::spawn(async move {
                // Wait for the guard to be dropped
                let _ = cancel_rx.await;

                // Clear the status
                let session = client.open_session(&token);
                let request = SlackApiAssistantThreadsSetStatusRequest::new(
                    channel_id,
                    String::new(),
                    thread_ts,
                );
                let _ = session.assistant_threads_set_status(&request).await;
            });

            TypingGuard::new(cancel_tx)
        } else {
            TypingGuard::noop()
        }
    }
}

// ============================================================================
// User State for Socket Mode
// ============================================================================

/// State passed to socket mode event handlers
struct SlackUserState {
    bot_token: SlackApiToken,
    /// Raw bot token string for file downloads (requires auth header)
    bot_token_str: String,
    bot_user_id: SlackUserId,
    task_manager: Arc<UserTaskManager>,
    /// Track the last thread_ts per user to detect "New Chat" clicks
    /// When thread_ts changes, we clear the Claude session
    user_threads: Arc<RwLock<HashMap<String, String>>>,
}

// ============================================================================
// Public API
// ============================================================================

/// Validate Slack credentials by calling auth.test
/// Returns the bot user ID on success
pub async fn validate_credentials(bot_token: &str, app_token: &str) -> Result<String> {
    // Ensure rustls crypto provider is installed
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Validate bot token
    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
    let token = SlackApiToken::new(bot_token.into());
    let session = client.open_session(&token);

    let response = session.auth_test().await?;
    let bot_user_id = response.user_id.to_string();

    // Validate app token format (basic check)
    if !app_token.starts_with("xapp-") {
        anyhow::bail!("App token should start with 'xapp-'");
    }

    Ok(bot_user_id)
}

/// Run the Slack bot using Socket Mode
pub async fn run(config: SlackConfig) -> Result<()> {
    // Ensure rustls crypto provider is installed
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    info!("Starting Slack bot...");

    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
    let bot_token = SlackApiToken::new(config.bot_token.clone().into());
    let app_token = SlackApiToken::new(config.app_token.clone().into());

    // Get bot user ID to filter out own messages
    let session = client.open_session(&bot_token);
    let auth_response = session.auth_test().await?;
    let bot_user_id = auth_response.user_id.clone();
    info!("Connected as bot user: {}", bot_user_id);

    // Create shared task manager for per-user message handling
    let task_manager = UserTaskManager::new();

    // Create user state
    let user_state = SlackUserState {
        bot_token: bot_token.clone(),
        bot_token_str: config.bot_token.clone(),
        bot_user_id,
        task_manager,
        user_threads: Arc::new(RwLock::new(HashMap::new())),
    };

    // Set up Socket Mode client with callbacks
    let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
        .with_push_events(handle_push_events)
        .with_interaction_events(handle_interaction_events)
        .with_command_events(handle_command_events);

    let listener_environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(client.clone()).with_user_state(user_state),
    );

    let socket_mode_listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment,
        socket_mode_callbacks,
    );

    socket_mode_listener.listen_for(&app_token).await?;
    socket_mode_listener.serve().await;

    Ok(())
}

// ============================================================================
// Event Handlers
// ============================================================================

async fn handle_push_events(
    event: SlackPushEventCallback,
    client: Arc<SlackHyperClient>,
    user_state_storage: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let SlackPushEventCallback { event, .. } = event;

    match event {
        SlackEventCallbackBody::Message(msg_event) => {
            // Get user state
            let states = user_state_storage.read().await;
            let user_state = states
                .get_user_state::<SlackUserState>()
                .ok_or("Missing user state")?;

            // Spawn message handling in background so we ack the event immediately
            // This prevents Slack from retrying delivery
            let bot_token = user_state.bot_token.clone();
            let bot_token_str = user_state.bot_token_str.clone();
            let bot_user_id = user_state.bot_user_id.clone();
            let task_manager = user_state.task_manager.clone();
            let user_threads = user_state.user_threads.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_message_event(
                    msg_event,
                    client,
                    bot_token,
                    bot_token_str,
                    bot_user_id,
                    task_manager,
                    user_threads,
                )
                .await
                {
                    warn!("Error handling Slack message: {}", e);
                }
            });
        }
        SlackEventCallbackBody::AssistantThreadStarted(thread_event) => {
            // User opened the assistant - send suggested prompts immediately
            let states = user_state_storage.read().await;
            let user_state = states
                .get_user_state::<SlackUserState>()
                .ok_or("Missing user state")?;

            let token = user_state.bot_token.clone();
            let channel_id = thread_event.assistant_thread.channel_id.clone();
            let thread_ts = thread_event.assistant_thread.thread_ts.clone();

            tokio::spawn(async move {
                set_suggested_prompts(&client, &token, &channel_id, &thread_ts).await;
            });
        }
        _ => {
            debug!("Ignoring event type: {:?}", event);
        }
    }

    Ok(())
}

async fn handle_message_event(
    event: SlackMessageEvent,
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    bot_token_str: String,
    bot_user_id: SlackUserId,
    task_manager: Arc<UserTaskManager>,
    user_threads: Arc<RwLock<HashMap<String, String>>>,
) -> Result<()> {
    // Skip messages from bots (including ourselves)
    if event.sender.bot_id.is_some() {
        return Ok(());
    }

    // Get user ID - skip if none
    let user_id = match &event.sender.user {
        Some(id) => id.clone(),
        None => return Ok(()),
    };

    // Skip own messages
    if user_id == bot_user_id {
        return Ok(());
    }

    // Get channel ID
    let channel_id = match &event.origin.channel {
        Some(id) => id.clone(),
        None => return Ok(()),
    };

    // Get thread_ts - this is crucial for AI Assistant apps
    // For AI apps, messages come with a thread_ts that we must reply to
    let thread_ts = event.origin.thread_ts.clone();

    // Get message text
    let text = match &event.content {
        Some(content) => content.text.clone().unwrap_or_default(),
        None => String::new(),
    };

    // Download any image files in the message
    let mut image_paths: Vec<PathBuf> = Vec::new();
    if let Some(content) = &event.content
        && let Some(files) = &content.files
    {
        for file in files {
            if is_image_file(file) {
                match download_slack_file(file, &bot_token_str).await {
                    Ok(path) => image_paths.push(path),
                    Err(e) => warn!("Failed to download Slack file: {}", e),
                }
            }
        }
    }

    // Skip if no text and no images
    if text.is_empty() && image_paths.is_empty() {
        return Ok(());
    }

    info!(
        "Message from {} in channel {} (thread: {:?}, ts: {}, subtype: {:?}): {}{}",
        user_id,
        channel_id,
        thread_ts,
        event.origin.ts,
        event.subtype,
        text,
        if image_paths.is_empty() {
            String::new()
        } else {
            format!(" [{} image(s)]", image_paths.len())
        }
    );

    // For Slack AI apps, we key Claude sessions by thread_ts, not just user ID
    // This allows users to have multiple conversations (threads) with separate contexts
    // When they return to an old thread via History, we load that thread's Claude session
    if let Some(ref ts) = thread_ts {
        let ts_str = ts.to_string();

        // Track current thread for this user (for logging/debugging)
        let mut threads = user_threads.write().await;
        let previous_thread = threads.insert(user_id.to_string(), ts_str.clone());

        let is_new_thread = previous_thread.as_ref() != Some(&ts_str);
        if is_new_thread {
            if previous_thread.is_some() {
                info!(
                    "User {} switched to thread {} (was: {:?})",
                    user_id, ts_str, previous_thread
                );
            } else {
                info!("User {} started thread {}", user_id, ts_str);
            }
        }
    }

    // Get user info for display name
    let (username, display_name) = get_user_info(&client, &token, &user_id).await;

    // Create channel wrapper with thread_ts for proper threading
    let channel: Arc<dyn Channel> = Arc::new(SlackChannel::new(
        client.clone(),
        token.clone(),
        channel_id.clone(),
        thread_ts.clone(),
    ));

    // For Slack, we use a composite user key that includes thread_ts
    // This allows each thread to have its own Claude session/context
    // Format: "user_id" for pairing/approval, "user_id:thread_ts" for sessions
    let user_id_str = user_id.to_string();
    let session_user_id = match &thread_ts {
        Some(ts) => format!("{}:{}", user_id, ts),
        None => user_id_str.clone(),
    };

    // Determine what action to take
    let mut store = PairingStore::load()?;

    // Use base user_id for pairing/approval checks (not thread-specific)
    let action = determine_action(
        channel.name(),
        &user_id_str,
        &text,
        &image_paths,
        &mut store,
        username,
        display_name,
    )?;

    // Execute the action - use session_user_id (includes thread) for Claude queries
    if let Some(query_text) = execute_action(channel.as_ref(), &user_id_str, action).await? {
        // QueryClaude action - queue with task manager for debouncing
        let text_with_images = build_text_with_images(&query_text, &image_paths);
        // Use thread-aware key for task manager too
        let user_key = format!("{}:{}", channel.name(), session_user_id);
        let channel_clone = channel.clone();
        let session_user_id_clone = session_user_id.clone();

        task_manager
            .process_message(user_key, text_with_images, move |messages| async move {
                // Use session_user_id so each thread gets its own Claude session
                execute_claude_query(channel_clone, &session_user_id_clone, messages).await;
            })
            .await;
    }

    Ok(())
}

/// Get user info from Slack API
async fn get_user_info(
    client: &Arc<SlackHyperClient>,
    token: &SlackApiToken,
    user_id: &SlackUserId,
) -> (Option<String>, Option<String>) {
    let session = client.open_session(token);

    match session
        .users_info(&SlackApiUsersInfoRequest::new(user_id.clone()))
        .await
    {
        Ok(response) => {
            let username = response.user.name.clone();
            let display_name = response
                .user
                .profile
                .as_ref()
                .and_then(|p| p.display_name.clone())
                .or_else(|| {
                    response
                        .user
                        .profile
                        .as_ref()
                        .and_then(|p| p.real_name.clone())
                });
            (username, display_name)
        }
        Err(e) => {
            warn!("Failed to get user info for {}: {}", user_id, e);
            (None, None)
        }
    }
}

async fn handle_interaction_events(
    _event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    _user_state_storage: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Handle interactive components (buttons, menus, etc.) if needed
    debug!("Received interaction event");
    Ok(())
}

async fn handle_command_events(
    _event: SlackCommandEvent,
    _client: Arc<SlackHyperClient>,
    _user_state_storage: SlackClientEventsUserState,
) -> Result<SlackCommandEventResponse, Box<dyn std::error::Error + Send + Sync>> {
    // Handle slash commands if needed
    debug!("Received command event");
    Ok(SlackCommandEventResponse::new(
        SlackMessageContent::new().with_text("OK".to_string()),
    ))
}
