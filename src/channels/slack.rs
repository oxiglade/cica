use anyhow::Result;
use async_trait::async_trait;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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

/// True for the `|---|:--:|` row that makes the line above it a table header.
fn is_table_separator(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|')
        && line.contains('-')
        && line
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn is_table_row(line: &str) -> bool {
    line.trim().starts_with('|')
}

/// Split `| a | b |` into its cells, dropping the empties the outer pipes create.
fn table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        // Inside a code block Slack renders no markup, so `*bold*` would show its
        // own asterisks. Strip the emphasis rather than convert it.
        .map(|c| c.replace("**", "").replace('`', "").trim().to_string())
        .collect()
}

fn cell(row: &[String], c: usize) -> &str {
    row.get(c).map(String::as_str).unwrap_or("")
}

fn looks_numeric(cell: &str) -> bool {
    let stripped: String = cell
        .chars()
        .filter(|c| !matches!(c, ',' | ' ' | '%' | '€' | '$' | '+'))
        .collect();
    !stripped.is_empty() && stripped.parse::<f64>().is_ok()
}

/// Render Markdown tables as fixed-width text in a code block.
///
/// Slack has no table support: a Markdown table arrives as its literal pipes and
/// dashes, which is unreadable exactly when the content is a comparison someone
/// needs to scan. A fenced block is the one place Slack guarantees a monospace
/// font, so padding the columns there is what makes the table line up.
fn tables_to_code_blocks(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_fence = false;

    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        // Already fenced, or not a header followed by a separator: pass through.
        if in_fence
            || !is_table_row(line)
            || i + 1 >= lines.len()
            || !is_table_separator(lines[i + 1])
        {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        let header = table_cells(line);
        let mut body: Vec<Vec<String>> = Vec::new();
        let mut j = i + 2;
        while j < lines.len() && is_table_row(lines[j]) && !is_table_separator(lines[j]) {
            body.push(table_cells(lines[j]));
            j += 1;
        }

        let width = body
            .iter()
            .map(Vec::len)
            .chain(std::iter::once(header.len()))
            .max()
            .unwrap_or(0);

        // Numbers read far better right-aligned, and a column of them is the
        // reason anyone asked for a table.
        let numeric: Vec<bool> = (0..width)
            .map(|c| {
                let mut any = false;
                for row in &body {
                    let v = cell(row, c);
                    if v.is_empty() {
                        continue;
                    }
                    if !looks_numeric(v) {
                        return false;
                    }
                    any = true;
                }
                any
            })
            .collect();

        let widths: Vec<usize> = (0..width)
            .map(|c| {
                std::iter::once(cell(&header, c))
                    .chain(body.iter().map(|row| cell(row, c)))
                    .map(|v| v.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let render = |row: &[String]| {
            (0..width)
                .map(|c| {
                    let v = cell(row, c);
                    let pad = widths[c].saturating_sub(v.chars().count());
                    if numeric[c] {
                        format!("{}{}", " ".repeat(pad), v)
                    } else {
                        format!("{}{}", v, " ".repeat(pad))
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };

        out.push("```".to_string());
        // A table whose first column is unlabelled produces an all-empty header
        // row; printing it would just be a blank line and a rule above nothing.
        if header.iter().any(|h| !h.is_empty()) {
            out.push(render(&header));
            out.push(
                widths
                    .iter()
                    .map(|w| "-".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("  ")
                    .trim_end()
                    .to_string(),
            );
        }
        for row in &body {
            out.push(render(row));
        }
        out.push("```".to_string());

        i = j;
    }

    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// A DM with no thread has no session to protect, so it keeps the per-user key.
fn dm_affinity_key(user_id: &str, thread_ts: Option<&SlackTs>) -> String {
    match thread_ts {
        Some(ts) => format!("{user_id}:{ts}"),
        None => user_id.to_string(),
    }
}

/// Where to post the reply. A DM sent outside an assistant thread carries no
/// `thread_ts`, and a reply posted without one lands as a top-level message the
/// assistant pane does not show — the user sees silence. Root the thread at their own
/// message instead.
///
/// **Reply routing only.** The event's own `thread_ts` stays untouched, because it also
/// keys the Claude session, the debounce id and the worker affinity. Substituting the
/// message ts there would mint a new session for every threadless DM — a fresh, empty
/// context on each message — and hand each one its own worker.
fn reply_thread(thread_ts: Option<&SlackTs>, message_ts: &SlackTs) -> SlackTs {
    thread_ts.cloned().unwrap_or_else(|| message_ts.clone())
}

/// Convert standard Markdown to Slack's mrkdwn format.
pub fn markdown_to_mrkdwn(text: &str) -> String {
    let mut result = tables_to_code_blocks(text);

    // Convert bold: **text** -> *text* via placeholder to avoid clobbering single asterisks.
    result = result.replace("**", "\x00BOLD\x00");
    result = result.replace("\x00BOLD\x00", "*");

    // Italic (*text* -> _text_) is intentionally skipped: conflicts with bullet points.

    // Convert links: [text](url) -> <url|text>
    let link_re = regex_lite::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = link_re.replace_all(&result, "<$2|$1>").to_string();

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

        // Convert markdown to Slack's mrkdwn format
        let mrkdwn_message = markdown_to_mrkdwn(message);

        // thread_ts is required for AI Assistant apps to reply in the correct thread.
        let mut request = SlackApiChatPostMessageRequest::new(
            self.channel_id.clone(),
            SlackMessageContent::new().with_text(mrkdwn_message),
        )
        .with_unfurl_links(self.unfurl_links)
        .with_unfurl_media(self.unfurl_links);

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
        Some(reply_thread(thread_ts.as_ref(), &event.origin.ts)),
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
            // Per thread, not per user. A DM thread is its own conversation with
            // its own session, so keying on the user alone made one person's
            // threads queue behind each other for no reason. Carried in the
            // `user` slot rather than a new variant, as the Linear channel does
            // with an issue id, so released workers can still decode it.
            user: dm_affinity_key(&user_id_str, thread_ts.as_ref()),
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
        Affinity, SlackTs, dm_affinity_key, get_slack_attachments_dir, markdown_to_mrkdwn,
        reply_thread, thinking_status,
    };
    use crate::channels::{assert_prompt_paths_resolve, build_text_with_images};
    use std::time::Duration;

    #[test]
    fn two_dm_threads_get_two_workers() {
        let a = Affinity::Chat {
            channel: "slack".into(),
            user: dm_affinity_key("U1", Some(&SlackTs("1789131057.645209".into()))),
        };
        let b = Affinity::Chat {
            channel: "slack".into(),
            user: dm_affinity_key("U1", Some(&SlackTs("1787905099.505869".into()))),
        };
        assert_ne!(
            a.id(),
            b.id(),
            "one user's two threads must not share a worker"
        );
    }

    #[test]
    fn the_same_thread_keeps_its_worker() {
        let ts = SlackTs("1789131057.645209".into());
        assert_eq!(
            Affinity::Chat {
                channel: "slack".into(),
                user: dm_affinity_key("U1", Some(&ts))
            }
            .id(),
            Affinity::Chat {
                channel: "slack".into(),
                user: dm_affinity_key("U1", Some(&ts))
            }
            .id()
        );
    }

    #[test]
    fn a_threadless_dm_still_keys_on_the_user() {
        // No thread means no session key either, so there is nothing to race on.
        assert_eq!(dm_affinity_key("U1", None), "U1");
    }

    #[test]
    fn two_users_in_the_same_thread_are_still_separate() {
        let ts = SlackTs("1789131057.645209".into());
        assert_ne!(
            dm_affinity_key("U1", Some(&ts)),
            dm_affinity_key("U2", Some(&ts))
        );
    }

    #[test]
    fn a_first_message_threads_under_itself() {
        // The one message guaranteed to have no thread is the first somebody sends,
        // which is exactly when a silent non-reply costs the most.
        let ts = SlackTs("1789472075.083909".into());
        assert_eq!(reply_thread(None, &ts), ts);
    }

    #[test]
    fn an_existing_thread_is_left_alone() {
        let thread = SlackTs("1789467241.350079".into());
        let message = SlackTs("1789472075.083909".into());
        assert_eq!(reply_thread(Some(&thread), &message), thread);
    }

    #[test]
    fn replying_in_a_thread_does_not_move_the_affinity_key() {
        // The reply lands under the user's own message, but the affinity stays
        // per-user — otherwise every threadless DM would get its own worker and
        // its own empty Claude session.
        let message = SlackTs("1789472075.083909".into());
        assert_eq!(reply_thread(None, &message), message);
        assert_eq!(dm_affinity_key("U1", None), "U1");
    }

    #[test]
    fn a_markdown_table_becomes_an_aligned_code_block() {
        // Verbatim from a production answer: Slack rendered these pipes literally.
        let input = "Comparison is against *57 Fleckvieh farms*:\n\n\
|  | Farm | p10 | p25 | p50 |\n\
|---|---|---|---|---|\n\
| **Total milk EF** | **0.84** | 0.84 | 0.88 | 1.02 |\n\
| Enteric fermentation | 0.41 | 0.45 | 0.50 | 0.55 |\n\
| Milk yield (kg FPCM/cow) | 8,403 | 7,782 | 8,088 | 8,372 |\n\n\
For emission factors lower is better.";
        let out = markdown_to_mrkdwn(input);
        assert!(out.contains("```"), "{out}");
        // Numbers right-aligned under their header, labels left-aligned.
        assert!(
            out.contains("Total milk EF              0.84   0.84   0.88   1.02"),
            "{out}"
        );
        assert!(
            out.contains("Milk yield (kg FPCM/cow)  8,403  7,782  8,088  8,372"),
            "{out}"
        );
        // No leftover pipes, and no emphasis asterisks inside the block.
        assert!(!out.contains('|'), "{out}");
        assert!(
            out.contains("                           Farm    p10    p25    p50"),
            "{out}"
        );
        assert!(out.contains("Comparison is against *57 Fleckvieh farms*"));
    }

    #[test]
    fn an_unlabelled_first_column_still_aligns() {
        // `|  | Value |` was the real shape: the first column has no header, the
        // second does, so the header row is kept and the blank simply pads.
        let input = "|  | Value |\n|---|---|\n| Best in supply base | 0.64 |\n| p10 | 0.84 |";
        let out = markdown_to_mrkdwn(input);
        assert_eq!(
            out,
            "```\n                     Value\n-------------------  -----\nBest in supply base   0.64\np10                   0.84\n```",
            "{out}"
        );
    }

    #[test]
    fn a_table_with_no_header_labels_at_all_skips_the_header_row() {
        let input = "|  |  |\n|---|---|\n| Rows | 12 |\n| Farms | 8 |";
        let out = markdown_to_mrkdwn(input);
        assert_eq!(out, "```\nRows   12\nFarms   8\n```", "{out}");
    }

    #[test]
    fn a_table_already_inside_a_code_block_is_left_alone() {
        let input = "```\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
        assert_eq!(markdown_to_mrkdwn(input), input);
    }

    #[test]
    fn prose_with_pipes_is_not_mistaken_for_a_table() {
        let input = "Use `a | b` for alternation.\nNot a table.";
        assert_eq!(
            markdown_to_mrkdwn(input),
            "Use `a | b` for alternation.\nNot a table."
        );
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
