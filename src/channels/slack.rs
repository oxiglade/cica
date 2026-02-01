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
use crate::config::SlackConfig;
use crate::pairing::PairingStore;

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
        // Slack doesn't support typing indicators for bots in the same way
        // as Telegram. We return a no-op guard.
        TypingGuard::noop()
    }
}

// ============================================================================
// User State for Socket Mode
// ============================================================================

/// State passed to socket mode event handlers
struct SlackUserState {
    bot_token: SlackApiToken,
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

            if let Err(e) = handle_message_event(
                msg_event,
                client,
                user_state.bot_token.clone(),
                user_state.bot_user_id.clone(),
                user_state.task_manager.clone(),
                user_state.user_threads.clone(),
            )
            .await
            {
                warn!("Error handling Slack message: {}", e);
            }
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

    // Skip empty messages
    if text.is_empty() {
        return Ok(());
    }

    info!(
        "Message from {} in channel {} (thread: {:?}): {}",
        user_id, channel_id, thread_ts, text
    );

    // Detect "New Chat" by checking if thread_ts changed for this user
    // This is a workaround since slack-morphism doesn't support assistant_thread_started yet
    if let Some(ref ts) = thread_ts {
        let user_key = user_id.to_string();
        let ts_str = ts.to_string();

        let mut threads = user_threads.write().await;
        let is_new_thread = threads
            .get(&user_key)
            .map(|old| old != &ts_str)
            .unwrap_or(true);

        if is_new_thread {
            info!(
                "New chat thread detected for user {}, clearing Claude session",
                user_id
            );
            threads.insert(user_key.clone(), ts_str);

            // Clear the Claude session (equivalent to /new command)
            let session_key = format!("slack:{}", user_id);
            if let Ok(mut store) = PairingStore::load() {
                store.sessions.remove(&session_key);
                if let Err(e) = store.save() {
                    warn!("Failed to save pairing store after clearing session: {}", e);
                }
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
        thread_ts,
    ));

    // Determine what action to take
    let mut store = PairingStore::load()?;
    let image_paths: Vec<PathBuf> = Vec::new(); // TODO: handle file attachments

    let action = determine_action(
        channel.name(),
        &user_id.to_string(),
        &text,
        &image_paths,
        &mut store,
        username,
        display_name,
    )?;

    // Execute the action
    if let Some(query_text) = execute_action(channel.as_ref(), &user_id.to_string(), action).await?
    {
        // QueryClaude action - queue with task manager for debouncing
        let text_with_images = build_text_with_images(&query_text, &image_paths);
        let user_key = format!("{}:{}", channel.name(), user_id);
        let channel_clone = channel.clone();
        let user_id_str = user_id.to_string();

        task_manager
            .process_message(user_key, text_with_images, move |messages| async move {
                execute_claude_query(channel_clone, &user_id_str, messages).await;
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
