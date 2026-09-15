use anyhow::Result;
use async_trait::async_trait;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::{
    Channel, Identity, TypingGuard, UserTaskManager, build_text_with_images, determine_action,
    execute_action, execute_claude_query,
};
use crate::config::{self, Paths, SlackConfig};
use crate::runtime::Runtime;
use crate::sandbox::Affinity;
use crate::skills;

fn get_slack_attachments_dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.internal_dir.join("slack_attachments");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

async fn download_slack_file(paths: &Paths, file: &SlackFile, bot_token: &str) -> Result<PathBuf> {
    let url = file
        .url_private_download
        .as_ref()
        .or(file.url_private.as_ref())
        .ok_or_else(|| anyhow::anyhow!("No download URL for file"))?;

    let file_name = file.name.as_deref().unwrap_or("unknown");
    let file_id = &file.id;

    let attachments_dir = get_slack_attachments_dir(paths)?;
    let local_path = attachments_dir.join(format!("{}_{}", file_id, file_name));

    if local_path.exists() {
        debug!("File already downloaded: {:?}", local_path);
        return Ok(local_path);
    }

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

fn is_image_file(file: &SlackFile) -> bool {
    file.mimetype
        .as_ref()
        .map(|m| m.to_string().starts_with("image/"))
        .unwrap_or(false)
}

async fn set_suggested_prompts(
    client: &Arc<SlackHyperClient>,
    token: &SlackApiToken,
    channel_id: &SlackChannelId,
    thread_ts: &SlackTs,
    paths: &Paths,
    prep_deps: bool,
) {
    let session = client.open_session(token);

    // Build prompts from available skills (up to 4, Slack's limit).
    let mut prompts = Vec::new();

    prompts.push(SlackAssistantPrompt::new(
        "What can you help me with?".to_string(),
        "What can you help me with?".to_string(),
    ));

    if let Ok(available_skills) = skills::discover_skills(paths, prep_deps) {
        for skill in available_skills.iter().take(3) {
            prompts.push(SlackAssistantPrompt::new(
                skill.description.clone(),
                skill.description.clone(),
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

const MARKDOWN_BLOCK_LIMIT: usize = 11_000;

pub fn markdown_message_chunks(text: &str) -> Vec<String> {
    const CLOSE_FENCE: &str = "\n```";

    let mut line_end = 0;
    let fence_ends: Vec<usize> = text
        .split_inclusive('\n')
        .filter_map(|line| {
            line_end += line.len();
            let trimmed = line.trim_start();
            trimmed
                .starts_with("```")
                .then_some(line_end - trimmed.len() + 3)
        })
        .collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut in_fence = false;

    loop {
        let prefix = if in_fence { "```\n" } else { "" };
        let remaining = &text[start..];
        let budget = MARKDOWN_BLOCK_LIMIT - prefix.len();
        let Some((mut max_end, _)) = remaining.char_indices().nth(budget) else {
            chunks.push(format!("{prefix}{remaining}"));
            break;
        };

        loop {
            let mut offset = 0;
            let mut blank_line = None;
            let mut newline = None;
            for line in remaining[..max_end].split_inclusive('\n') {
                offset += line.len();
                if line.ends_with('\n') {
                    newline = Some(offset);
                    if line.trim().is_empty() {
                        blank_line = Some(offset);
                    }
                }
            }
            let mut end = start + blank_line.or(newline).unwrap_or(max_end);

            let fence_count = fence_ends.partition_point(|&fence_end| fence_end <= end);
            if let Some(&fence_end) = fence_ends.get(fence_count) {
                end = end.min(fence_end - 3);
            }
            let next_in_fence = !fence_count.is_multiple_of(2);
            if next_in_fence && text[start..end].chars().count() + CLOSE_FENCE.len() > budget {
                max_end = remaining
                    .char_indices()
                    .nth(budget - CLOSE_FENCE.len())
                    .unwrap()
                    .0;
                continue;
            }

            let suffix = if next_in_fence { CLOSE_FENCE } else { "" };
            chunks.push(format!("{prefix}{}{suffix}", &text[start..end]));
            start = end;
            in_fence = next_in_fence;
            break;
        }
    }

    chunks
}

static MARKDOWN_LINK_RE: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

/// Convert standard Markdown to Slack's mrkdwn format.
pub fn markdown_to_mrkdwn(text: &str) -> String {
    let mut result = text.to_string();

    // Convert bold: **text** -> *text* via placeholder to avoid clobbering single asterisks.
    result = result.replace("**", "\x00BOLD\x00");
    result = result.replace("\x00BOLD\x00", "*");

    // Italic (*text* -> _text_) is intentionally skipped: conflicts with bullet points.

    // Convert links: [text](url) -> <url|text>
    result = MARKDOWN_LINK_RE.replace_all(&result, "<$2|$1>").to_string();

    result
}

const STATUS_REFRESH: Duration = Duration::from_secs(10);

fn thinking_status(elapsed: Duration) -> String {
    match elapsed.as_secs() / 60 {
        0 => "is thinking...".to_string(),
        mins => format!("is thinking... ({mins}m)"),
    }
}

/// Slack channel implementation for AI Assistant threads.
pub struct SlackChannel {
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    /// The DM channel ID
    channel_id: SlackChannelId,
    /// Thread timestamp - required for AI Assistant apps to reply in the correct thread
    thread_ts: Option<SlackTs>,
    /// Whether to allow Slack to unfurl (preview) links in messages
    unfurl_links: bool,
}

impl SlackChannel {
    pub fn new(
        client: Arc<SlackHyperClient>,
        token: SlackApiToken,
        channel_id: SlackChannelId,
        thread_ts: Option<SlackTs>,
        unfurl_links: bool,
    ) -> Self {
        Self {
            client,
            token,
            channel_id,
            thread_ts,
            unfurl_links,
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

        for chunk in markdown_message_chunks(message) {
            let content = SlackMessageContent::new()
                .with_text(markdown_to_mrkdwn(&chunk))
                .with_blocks(vec![SlackMarkdownBlock::new(chunk).into()]);
            let mut request = SlackApiChatPostMessageRequest::new(self.channel_id.clone(), content)
                .with_unfurl_links(self.unfurl_links)
                .with_unfurl_media(self.unfurl_links);

            if let Some(ts) = &self.thread_ts {
                request = request.with_thread_ts(ts.clone());
            }

            debug!("Request: {:?}", request);

            match session.chat_post_message(&request).await {
                Ok(response) => {
                    info!("Message sent successfully, ts: {:?}", response.ts);
                }
                Err(e) => {
                    warn!("Failed to send message: {}", e);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    async fn send_message_with_attachments(
        &self,
        message: &str,
        attachment_paths: &[PathBuf],
    ) -> Result<()> {
        if attachment_paths.is_empty() {
            return self.send_message(message).await;
        }

        let session = self.client.open_session(&self.token);

        let mut uploaded_files = Vec::new();

        for path in attachment_paths {
            if !path.exists() {
                warn!("Attachment path does not exist: {:?}", path);
                continue;
            }

            let file_bytes = std::fs::read(path)?;
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();

            let get_url_req =
                SlackApiFilesGetUploadUrlExternalRequest::new(filename.clone(), file_bytes.len());

            let get_url_resp = session
                .get_upload_url_external(&get_url_req)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get upload URL: {}", e))?;

            let content_type = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();

            let upload_req = SlackApiFilesUploadViaUrlRequest::new(
                get_url_resp.upload_url,
                file_bytes,
                content_type,
            );

            session
                .files_upload_via_url(&upload_req)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to upload file: {}", e))?;

            uploaded_files
                .push(SlackApiFilesComplete::new(get_url_resp.file_id).with_title(filename));
        }

        if uploaded_files.is_empty() {
            if !message.is_empty() {
                return self.send_message(message).await;
            }
            return Ok(());
        }

        let mut complete_req = SlackApiFilesCompleteUploadExternalRequest::new(uploaded_files)
            .with_channel_id(self.channel_id.clone());

        if !message.is_empty() {
            // File uploads accept only an initial_comment string, not Markdown blocks.
            let mrkdwn_message = markdown_to_mrkdwn(message);
            complete_req = complete_req.with_initial_comment(mrkdwn_message);
        }

        if let Some(ts) = &self.thread_ts {
            complete_req = complete_req.with_thread_ts(ts.clone());
        }

        session
            .files_complete_upload_external(&complete_req)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to complete file upload: {}", e))?;

        info!("Sent message with attachments to Slack");
        Ok(())
    }

    fn start_typing(&self) -> TypingGuard {
        // Uses assistant.threads.setStatus to show a "thinking" indicator.
        if let Some(thread_ts) = &self.thread_ts {
            let client = self.client.clone();
            let token = self.token.clone();
            let channel_id = self.channel_id.clone();
            let thread_ts = thread_ts.clone();

            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();

            // Re-asserted with elapsed minutes so a long turn reads as running, not crashed.
            tokio::spawn(async move {
                let session = client.open_session(&token);
                let started = Instant::now();
                let mut status = thinking_status(Duration::ZERO);

                loop {
                    let request = SlackApiAssistantThreadsSetStatusRequest::new(
                        channel_id.clone(),
                        status.clone(),
                        thread_ts.clone(),
                    );
                    if let Err(e) = session.assistant_threads_set_status(&request).await {
                        warn!("Failed to set assistant status: {}", e);
                    }

                    tokio::select! {
                        _ = tokio::time::sleep(STATUS_REFRESH) => {
                            status = thinking_status(started.elapsed());
                        }
                        _ = &mut cancel_rx => {
                            let request = SlackApiAssistantThreadsSetStatusRequest::new(
                                channel_id,
                                String::new(),
                                thread_ts,
                            );
                            let _ = session.assistant_threads_set_status(&request).await;
                            break;
                        }
                    }
                }
            });

            TypingGuard::new(cancel_tx)
        } else {
            TypingGuard::noop()
        }
    }
}

/// State passed to socket mode event handlers.
#[derive(Clone)]
struct SlackUserState {
    rt: Arc<Runtime>,
    bot_token: SlackApiToken,
    /// Raw bot token string for file downloads (requires auth header)
    bot_token_str: String,
    bot_user_id: SlackUserId,
    task_manager: Arc<UserTaskManager>,
    /// Track the last thread_ts per user to detect "New Chat" clicks
    /// When thread_ts changes, we clear the Claude session
    user_threads: Arc<RwLock<HashMap<String, String>>>,
    /// Whether to allow Slack to unfurl (preview) links in messages
    unfurl_links: bool,
}

/// Validate Slack credentials by calling auth.test; returns the bot user ID on success.
pub async fn validate_credentials(bot_token: &str, app_token: &str) -> Result<String> {
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

/// Run the Slack bot using Socket Mode.
pub async fn run(config: SlackConfig, rt: Arc<Runtime>) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    info!("Starting Slack bot...");

    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
    let bot_token = SlackApiToken::new(config.bot_token.clone().into());
    let app_token = SlackApiToken::new(config.app_token.clone().into());

    let session = client.open_session(&bot_token);
    let auth_response = session.auth_test().await?;
    let bot_user_id = auth_response.user_id.clone();
    info!("Connected as bot user: {}", bot_user_id);

    let task_manager = UserTaskManager::new();

    let user_state = SlackUserState {
        rt,
        bot_token: bot_token.clone(),
        bot_token_str: config.bot_token.clone(),
        bot_user_id,
        task_manager,
        user_threads: Arc::new(RwLock::new(HashMap::new())),
        unfurl_links: config.unfurl_links,
    };

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

async fn handle_push_events(
    event: SlackPushEventCallback,
    client: Arc<SlackHyperClient>,
    user_state_storage: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let SlackPushEventCallback { event, .. } = event;

    match event {
        SlackEventCallbackBody::Message(msg_event) => {
            let states = user_state_storage.read().await;
            let user_state = states
                .get_user_state::<SlackUserState>()
                .ok_or("Missing user state")?;

            // Spawn in background so we ack the event immediately and prevent Slack retries.
            let state = user_state.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_message_event(msg_event, client, state).await {
                    warn!("Error handling Slack message: {}", e);
                }
            });
        }
        SlackEventCallbackBody::AssistantThreadStarted(thread_event) => {
            let states = user_state_storage.read().await;
            let user_state = states
                .get_user_state::<SlackUserState>()
                .ok_or("Missing user state")?;

            let token = user_state.bot_token.clone();
            let channel_id = thread_event.assistant_thread.channel_id.clone();
            let thread_ts = thread_event.assistant_thread.thread_ts.clone();
            let paths = user_state.rt.paths.clone();
            let prep_deps =
                config::prep_skill_deps_locally(user_state.rt.config.deployment.provider);

            tokio::spawn(async move {
                set_suggested_prompts(&client, &token, &channel_id, &thread_ts, &paths, prep_deps)
                    .await;
            });
        }
        SlackEventCallbackBody::AppMention(mention_event) => {
            let states = user_state_storage.read().await;
            let user_state = states
                .get_user_state::<SlackUserState>()
                .ok_or("Missing user state")?;
            let state = user_state.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_app_mention_event(mention_event, client, state).await {
                    warn!("Error handling Slack app mention: {}", e);
                }
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
    state: SlackUserState,
) -> Result<()> {
    if event.sender.bot_id.is_some() {
        return Ok(());
    }

    let user_id = match &event.sender.user {
        Some(id) => id.clone(),
        None => return Ok(()),
    };

    if user_id == state.bot_user_id {
        return Ok(());
    }

    let channel_id = match &event.origin.channel {
        Some(id) => id.clone(),
        None => return Ok(()),
    };

    // Only process DMs here; channel messages require an @mention (handle_app_mention_event).
    let channel_str = channel_id.to_string();
    if !channel_str.starts_with('D') {
        return Ok(());
    }

    let thread_ts = event.origin.thread_ts.clone();

    let text = match &event.content {
        Some(content) => content.text.clone().unwrap_or_default(),
        None => String::new(),
    };

    let mut image_paths: Vec<PathBuf> = Vec::new();
    if let Some(content) = &event.content
        && let Some(files) = &content.files
    {
        for file in files {
            if is_image_file(file) {
                match download_slack_file(&state.rt.paths, file, &state.bot_token_str).await {
                    Ok(path) => image_paths.push(path),
                    Err(e) => warn!("Failed to download Slack file: {}", e),
                }
            }
        }
    }

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
        let mut threads = state.user_threads.write().await;
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

    let (username, display_name) = get_user_info(&client, &state.bot_token, &user_id).await;

    let channel: Arc<dyn Channel> = Arc::new(SlackChannel::new(
        client.clone(),
        state.bot_token.clone(),
        channel_id.clone(),
        thread_ts.clone(),
        state.unfurl_links,
    ));

    // Each DM thread gets its own Claude session (keyed user_id:thread_ts).
    let user_id_str = user_id.to_string();
    let session_key = thread_ts
        .as_ref()
        .map(|ts| format!("slack:{}:{}", user_id, ts));
    let debounce_id = match &thread_ts {
        Some(ts) => format!("{}:{}:{}", channel.name(), user_id, ts),
        None => format!("{}:{}", channel.name(), user_id),
    };

    let action = determine_action(
        &state.rt,
        channel.name(),
        &user_id_str,
        &text,
        &image_paths,
        username,
        display_name,
        session_key.as_deref(),
    )?;

    if let Some(query_text) =
        execute_action(&state.rt, channel.as_ref(), &user_id_str, action).await?
    {
        let (text_with_images, attachment_names) =
            build_text_with_images(&state.rt.paths.base, &query_text, &image_paths);
        let channel_clone = channel.clone();
        let identity = Identity::of(channel.as_ref(), &user_id_str);
        let session_key_clone = session_key.clone();
        let affinity = Affinity::Chat {
            channel: channel.name().to_string(),
            user: user_id_str.clone(),
        };
        let rt = state.rt.clone();

        state
            .task_manager
            .process_message(debounce_id, text_with_images, move |messages| async move {
                execute_claude_query(
                    rt,
                    channel_clone,
                    &identity,
                    affinity,
                    messages,
                    session_key_clone,
                    attachment_names,
                )
                .await;
            })
            .await;
    }

    Ok(())
}

/// Fetch thread messages that the bot hasn't seen yet.
///
/// On first mention (no prior bot replies), returns all thread messages as context.
/// On subsequent mentions, returns only messages after the bot's last reply.
/// Messages are formatted as `[speaker]: text` lines.
const THREAD_CONTEXT_LIMIT: u16 = 50;

async fn fetch_thread_context(
    client: &Arc<SlackHyperClient>,
    token: &SlackApiToken,
    channel_id: &SlackChannelId,
    thread_ts: &SlackTs,
    bot_user_id: &SlackUserId,
    current_msg_ts: &SlackTs,
) -> Vec<String> {
    let session = client.open_session(token);
    let request = SlackApiConversationsRepliesRequest::new(channel_id.clone(), thread_ts.clone())
        .with_limit(THREAD_CONTEXT_LIMIT);

    let replies = match session.conversations_replies(&request).await {
        Ok(response) => response.messages,
        Err(e) => {
            warn!("Failed to fetch thread replies: {}", e);
            return Vec::new();
        }
    };

    // Find the bot's last message timestamp to use as a watermark
    let bot_last_ts = replies
        .iter()
        .rev()
        .find(|msg| msg.sender.user.as_ref() == Some(bot_user_id))
        .map(|msg| &msg.origin.ts);

    // Collect non-bot messages after the watermark (or all if no bot reply yet)
    replies
        .iter()
        .filter(|msg| {
            // Skip the current message (it's already being sent)
            if msg.origin.ts == *current_msg_ts {
                return false;
            }
            // Skip bot's own messages
            if msg.sender.user.as_ref() == Some(bot_user_id) {
                return false;
            }
            // If bot has replied before, only include messages after its last reply
            if let Some(watermark) = bot_last_ts {
                return msg.origin.ts.to_string() > watermark.to_string();
            }
            true
        })
        .map(|msg| {
            let speaker = msg
                .sender
                .user
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let text = msg.content.text.clone().unwrap_or_default();
            format!("[{}]: {}", speaker, text)
        })
        .collect()
}

/// Handle @mention events in channels
async fn handle_app_mention_event(
    event: SlackAppMentionEvent,
    client: Arc<SlackHyperClient>,
    state: SlackUserState,
) -> Result<()> {
    let user_id = event.user.clone();
    let channel_id = event.channel.clone();

    let text = event
        .content
        .text
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();

    if text.is_empty() {
        return Ok(());
    }

    // Use the existing thread_ts if replying in a thread; otherwise start one from this message's ts.
    let thread_ts = event
        .origin
        .thread_ts
        .clone()
        .unwrap_or_else(|| event.origin.ts.clone());

    info!(
        "App mention from {} in channel {} (thread: {}): {}",
        user_id, channel_id, thread_ts, text
    );

    let user_id_str = user_id.to_string();
    let user_info = if crate::runtime::lock(&state.rt.pairing).is_approved("slack", &user_id_str) {
        None
    } else {
        Some(get_user_info(&client, &state.bot_token, &user_id).await)
    };
    let needs_pairing = {
        let mut store = crate::runtime::lock(&state.rt.pairing);
        if !store.is_approved("slack", &user_id_str) {
            store.reload()?;
        }
        if store.is_approved("slack", &user_id_str) {
            false
        } else if state.rt.config.channel_settings("slack").auto_approve {
            let (username, display_name) = user_info.unwrap_or_default();
            store.modify(|store| {
                store.auto_approve("slack", &user_id_str, username, display_name)
            })?;
            false
        } else {
            true
        }
    };
    if needs_pairing {
        send_ephemeral_message(
            &client,
            &state.bot_token,
            &channel_id,
            &user_id,
            "Hi! I don't recognize you yet. Please send me a direct message to get started.",
        )
        .await;
        return Ok(());
    }

    let settings = state.rt.config.channel_settings("slack");
    let onboarding_complete =
        crate::onboarding::is_complete_for_user(&state.rt.paths, &settings, "slack", &user_id_str)?;
    if !onboarding_complete {
        send_ephemeral_message(
            &client,
            &state.bot_token,
            &channel_id,
            &user_id,
            "Tip: Send me a direct message to set up your profile for more personalized responses.",
        )
        .await;
    }

    {
        let ts_str = thread_ts.to_string();
        let mut threads = state.user_threads.write().await;
        let previous_thread = threads.insert(user_id.to_string(), ts_str.clone());

        if previous_thread.as_ref() != Some(&ts_str) {
            info!(
                "User {} started/joined thread {} in channel {}",
                user_id, ts_str, channel_id
            );
        }
    }

    let mut image_paths: Vec<PathBuf> = Vec::new();
    if let Some(files) = &event.content.files {
        for file in files {
            if is_image_file(file) {
                match download_slack_file(&state.rt.paths, file, &state.bot_token_str).await {
                    Ok(path) => image_paths.push(path),
                    Err(e) => warn!("Failed to download Slack file: {}", e),
                }
            }
        }
    }

    let (_, display_name) = get_user_info(&client, &state.bot_token, &user_id).await;
    let speaker_name = display_name.unwrap_or_else(|| user_id.to_string());

    let unseen_context = fetch_thread_context(
        &client,
        &state.bot_token,
        &channel_id,
        &thread_ts,
        &state.bot_user_id,
        &event.origin.ts,
    )
    .await;

    let channel: Arc<dyn Channel> = Arc::new(SlackChannel::new(
        client.clone(),
        state.bot_token.clone(),
        channel_id.clone(),
        Some(thread_ts.clone()),
        state.unfurl_links,
    ));

    // All users in the same public thread share one Claude session so context
    // carries across speakers.
    let shared_session_key = format!("slack:thread:{}:{}", channel_id, thread_ts);
    let user_id_str = user_id.to_string();

    if text == "/new"
        && let super::CommandResult::Response(response) = super::process_command(
            &state.rt,
            channel.name(),
            &user_id_str,
            &text,
            onboarding_complete,
            Some(&shared_session_key),
        )?
    {
        channel.send_message(&response).await?;
        return Ok(());
    }

    let current_msg = format!("[{}]: {}", speaker_name, text);
    let full_text = if unseen_context.is_empty() {
        current_msg
    } else {
        format!(
            "[thread context]\n{}\n[/thread context]\n\n{}",
            unseen_context.join("\n"),
            current_msg
        )
    };
    let (text_with_images, attachment_names) =
        build_text_with_images(&state.rt.paths.base, &full_text, &image_paths);

    // Debounce per user so rapid messages from one person batch, but session is shared.
    let user_key = format!("{}:{}:{}", channel.name(), user_id, thread_ts);
    let channel_clone = channel.clone();
    let identity = Identity::of(channel.as_ref(), &user_id_str);
    let session_key = Some(shared_session_key);
    let affinity = Affinity::SlackThread {
        channel_id: channel_id.to_string(),
        thread_ts: thread_ts.to_string(),
    };
    let rt = state.rt.clone();

    state
        .task_manager
        .process_message(user_key, text_with_images, move |messages| async move {
            execute_claude_query(
                rt,
                channel_clone,
                &identity,
                affinity,
                messages,
                session_key,
                attachment_names,
            )
            .await;
        })
        .await;

    Ok(())
}

async fn send_ephemeral_message(
    client: &Arc<SlackHyperClient>,
    token: &SlackApiToken,
    channel_id: &SlackChannelId,
    user_id: &SlackUserId,
    message: &str,
) {
    let session = client.open_session(token);

    let request = SlackApiChatPostEphemeralRequest::new(
        channel_id.clone(),
        user_id.clone(),
        SlackMessageContent::new().with_text(message.to_string()),
    );

    if let Err(e) = session.chat_post_ephemeral(&request).await {
        warn!("Failed to send ephemeral message: {}", e);
    }
}

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
    debug!("Received interaction event");
    Ok(())
}

async fn handle_command_events(
    _event: SlackCommandEvent,
    _client: Arc<SlackHyperClient>,
    _user_state_storage: SlackClientEventsUserState,
) -> Result<SlackCommandEventResponse, Box<dyn std::error::Error + Send + Sync>> {
    debug!("Received command event");
    Ok(SlackCommandEventResponse::new(
        SlackMessageContent::new().with_text("OK".to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        MARKDOWN_BLOCK_LIMIT, get_slack_attachments_dir, markdown_message_chunks,
        markdown_to_mrkdwn, thinking_status,
    };
    use crate::channels::{assert_prompt_paths_resolve, build_text_with_images};
    use std::time::Duration;

    #[test]
    fn markdown_chunks_handle_fences_at_the_boundary() {
        let strip = |s: &str| -> String {
            s.chars()
                .filter(|c| *c != '\n' && *c != '`' && *c != '\r')
                .collect()
        };
        let mut checked = 0;
        for offset in (MARKDOWN_BLOCK_LIMIT - 30)..(MARKDOWN_BLOCK_LIMIT + 30) {
            for shape in 0..6 {
                let head = "a".repeat(offset);
                let input = match shape {
                    0 => format!("{head}```\n{}", "b".repeat(500)),
                    1 => format!("{head}\n```\n{}\n```\n", "b".repeat(500)),
                    2 => format!("```\n{head}\n```\n{}", "b".repeat(500)),
                    3 => format!("{head}```"),
                    4 => format!("```rust\n{head}{}", "b".repeat(500)),
                    _ => format!("{head}\n   ```   \n{}", "b".repeat(500)),
                };
                let chunks = markdown_message_chunks(&input);
                checked += 1;
                assert!(!chunks.is_empty(), "offset {offset} shape {shape}: none");
                for (n, chunk) in chunks.iter().enumerate() {
                    assert!(
                        chunk.chars().count() <= MARKDOWN_BLOCK_LIMIT,
                        "offset {offset} shape {shape}: chunk {n} is {} chars",
                        chunk.chars().count()
                    );
                    assert!(
                        !chunk.is_empty(),
                        "offset {offset} shape {shape}: chunk {n} empty"
                    );
                }
                assert_eq!(
                    strip(&chunks.concat()),
                    strip(&input),
                    "offset {offset} shape {shape}: content changed"
                );
            }
        }
        assert!(checked > 300, "{checked}");
    }

    #[test]
    fn markdown_chunks_survive_arbitrary_input() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let strip = |s: &str| -> String {
            s.chars()
                .filter(|c| *c != '\n' && *c != '`' && *c != '\r')
                .collect()
        };
        for case in 0..2000 {
            let mut input = String::new();
            let pieces = (rng() % 40) as usize;
            for _ in 0..pieces {
                match rng() % 10 {
                    0 => input.push_str("```\n"),
                    1 => input.push_str("```rust\n"),
                    2 => input.push('\n'),
                    3 => input.push_str("\n\n"),
                    4 => input.push_str(&"x".repeat((rng() % 4000) as usize)),
                    5 => input.push('🦀'),
                    6 => input.push_str("  ```  \n"),
                    7 => input.push_str(&"é".repeat((rng() % 4000) as usize)),
                    8 => input.push_str("\n \t\n"),
                    _ => input.push_str("some text "),
                }
            }
            if input.is_empty() {
                continue;
            }
            let chunks = markdown_message_chunks(&input);
            assert!(!chunks.is_empty(), "case {case}: no chunks");
            for (n, chunk) in chunks.iter().enumerate() {
                assert!(
                    chunk.chars().count() <= MARKDOWN_BLOCK_LIMIT,
                    "case {case}: chunk {n} is {} chars",
                    chunk.chars().count()
                );
                assert!(!chunk.is_empty(), "case {case}: chunk {n} is empty");
            }
            assert_eq!(
                strip(&chunks.concat()),
                strip(&input),
                "case {case}: content changed"
            );
        }
    }

    #[test]
    fn markdown_chunks_leave_short_text_unchanged() {
        for input in [
            String::new(),
            "**Hello**\n\n```rust\nlet x = 1;\n```\n".to_string(),
            "🦀".repeat(MARKDOWN_BLOCK_LIMIT),
        ] {
            assert_eq!(markdown_message_chunks(&input), vec![input]);
        }
    }

    #[test]
    fn markdown_chunks_prefer_blank_lines() {
        for separator in ["\n\n", "\r\n\r\n", "\n \t\n"] {
            let first = format!("{}{separator}", "a".repeat(6000));
            let second = format!("{}\n{}", "b".repeat(2000), "c".repeat(6000));
            let input = format!("{first}{second}");
            let chunks = markdown_message_chunks(&input);

            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks.concat(), input);
            assert_eq!(chunks, vec![first, second]);
        }
    }

    #[test]
    fn markdown_chunks_split_at_newlines_without_blank_lines() {
        let first = format!("{}\n", "a".repeat(6000));
        let second = "b".repeat(6000);
        let input = format!("{first}{second}");
        let chunks = markdown_message_chunks(&input);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), input);
        assert_eq!(chunks, vec![first, second]);
    }

    #[test]
    fn markdown_chunks_use_line_breaks_at_the_limit() {
        for first in [
            format!("{}\n\n", "x".repeat(MARKDOWN_BLOCK_LIMIT - 2)),
            format!("{}\n", "x".repeat(MARKDOWN_BLOCK_LIMIT - 1)),
            format!("```\n{}\n```\n\n", "x".repeat(MARKDOWN_BLOCK_LIMIT - 10)),
        ] {
            let input = format!("{first}tail");
            let chunks = markdown_message_chunks(&input);

            assert_eq!(chunks[0].chars().count(), MARKDOWN_BLOCK_LIMIT);
            assert_eq!(chunks, vec![first, "tail".to_string()]);
        }
    }

    #[test]
    fn markdown_chunks_hard_split_at_character_boundaries() {
        let input = "é🦀e\u{0301}".repeat(MARKDOWN_BLOCK_LIMIT);
        let chunks = markdown_message_chunks(&input);

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= MARKDOWN_BLOCK_LIMIT)
        );
        assert_eq!(chunks.concat(), input);
    }

    #[test]
    fn markdown_chunks_close_and_reopen_code_fences() {
        for body in [
            "let value = 1;\n\n".repeat(2000),
            "🦀".repeat(MARKDOWN_BLOCK_LIMIT * 2 + 1),
        ] {
            let input = format!("```rust\n{body}\n```");
            let chunks = markdown_message_chunks(&input);
            assert!(chunks.len() > 2);

            let mut recovered = String::new();
            for (i, chunk) in chunks.iter().enumerate() {
                assert!(chunk.chars().count() <= MARKDOWN_BLOCK_LIMIT);
                let mut original = chunk.as_str();
                if i > 0 {
                    original = original.strip_prefix("```\n").unwrap();
                }
                if i + 1 < chunks.len() {
                    original = original.strip_suffix("\n```").unwrap();
                }
                recovered.push_str(original);
            }
            assert_eq!(recovered, input);
        }
    }

    #[test]
    fn markdown_chunks_leave_complete_code_fences_alone() {
        let block = format!("Before\n```rust\n{}\n```\n\n", "x".repeat(6000));
        let input = block.repeat(2);
        let chunks = markdown_message_chunks(&input);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), input);
        assert_eq!(chunks, vec![block.clone(), block]);
    }

    #[test]
    fn markdown_table_passes_through_unchanged() {
        let input = "| A | B |\n|---|---|\n| x | 1 |\n| y | 2 |\n";
        assert_eq!(markdown_to_mrkdwn(input), input);
    }

    #[test]
    fn bold_and_links_still_convert_outside_tables() {
        let input = "**bold** and [text](https://example.com)";
        assert_eq!(
            markdown_to_mrkdwn(input),
            "*bold* and <https://example.com|text>"
        );
    }
    #[test]
    fn slack_download_location_resolves_in_the_prompt() {
        let (_temp, paths) = crate::config::test_paths();
        let attachment = get_slack_attachments_dir(&paths)
            .unwrap()
            .join("F1_shot.png");
        std::fs::write(&attachment, "fixture").unwrap();

        let prompt = build_text_with_images(&paths.base, "look", &[attachment]);

        assert_prompt_paths_resolve(&paths.base, &prompt);
    }

    #[test]
    fn shows_no_minutes_under_a_minute() {
        assert_eq!(thinking_status(Duration::from_secs(0)), "is thinking...");
        assert_eq!(thinking_status(Duration::from_secs(59)), "is thinking...");
    }

    #[test]
    fn counts_whole_minutes() {
        assert_eq!(
            thinking_status(Duration::from_secs(60)),
            "is thinking... (1m)"
        );
        assert_eq!(
            thinking_status(Duration::from_secs(570)),
            "is thinking... (9m)"
        );
    }
}
