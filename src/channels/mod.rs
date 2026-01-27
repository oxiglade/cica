pub mod signal;
pub mod telegram;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::claude::{self, QueryOptions};
use crate::cron::{
    self, CronSchedule, CronStore, format_timestamp, parse_add_command, truncate_for_name,
};
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::pairing::PairingStore;
use crate::skills;

// ============================================================================
// Channel Abstraction
// ============================================================================

/// Abstraction over channel-specific transport operations.
///
/// Each channel (Telegram, Signal, etc.) implements this trait to provide
/// a unified interface for sending messages and showing typing indicators.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Channel identifier (e.g., "telegram", "signal")
    fn name(&self) -> &'static str;

    /// Display name for user-facing messages (e.g., "Telegram", "Signal")
    fn display_name(&self) -> &'static str;

    /// Send a text message to the user
    async fn send_message(&self, message: &str) -> Result<()>;

    /// Start a typing indicator. Returns a guard that stops the indicator when dropped.
    fn start_typing(&self) -> TypingGuard;
}

/// RAII guard for typing indicators.
///
/// The typing indicator runs until this guard is dropped.
pub struct TypingGuard {
    cancel: Option<oneshot::Sender<()>>,
}

impl TypingGuard {
    /// Create a new typing guard with a cancel channel
    pub fn new(cancel: oneshot::Sender<()>) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

    /// Create a no-op guard (for testing or when typing indicators aren't supported)
    #[allow(dead_code)]
    pub fn noop() -> Self {
        Self { cancel: None }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

// ============================================================================
// Message Actions
// ============================================================================

/// Actions that can result from processing an incoming message.
///
/// This enum represents "what to do" without "how to do it", enabling
/// pure logic in `determine_action()` that's easy to test.
pub enum MessageAction {
    /// Send a simple response (command output, error message, etc.)
    SendResponse(String),

    /// Execute a cron job immediately
    ExecuteCronJob { job_id: String },

    /// Run onboarding flow with Claude
    Onboarding { message: String },

    /// Query Claude with the user's message
    QueryClaude { text: String },

    /// User not approved - send pairing instructions
    NeedsPairing { code: String },

    /// No action needed (empty message, /start after onboarding, etc.)
    Ignore,
}

/// Determine what action to take for an incoming message.
///
/// This is a pure function with no side effects - it only reads state and
/// returns what should happen. This makes it easy to test.
pub fn determine_action(
    channel: &str,
    user_id: &str,
    text: &str,
    _image_paths: &[PathBuf],
    store: &mut PairingStore,
    username: Option<String>,
    display_name: Option<String>,
) -> Result<MessageAction> {
    let text = text.trim();

    // Check if user is approved
    if !store.is_approved(channel, user_id) {
        let (code, _is_new) =
            store.get_or_create_pending(channel, user_id, username, display_name)?;
        return Ok(MessageAction::NeedsPairing { code });
    }

    // Check if onboarding is complete
    let onboarding_complete = onboarding::is_complete_for_user(channel, user_id)?;

    // Process commands (work even during onboarding)
    match process_command(store, channel, user_id, text, onboarding_complete)? {
        CommandResult::Response(response) => {
            return Ok(MessageAction::SendResponse(response));
        }
        CommandResult::CronRun(job_id) => {
            return Ok(MessageAction::ExecuteCronJob { job_id });
        }
        CommandResult::NotACommand => {}
    }

    // Handle onboarding if not complete
    if !onboarding_complete {
        // Treat /start as "hi" for onboarding
        let message = if text == "/start" { "hi" } else { text };
        return Ok(MessageAction::Onboarding {
            message: message.to_string(),
        });
    }

    // Ignore /start after onboarding
    if text == "/start" {
        return Ok(MessageAction::Ignore);
    }

    // Empty message with no images - ignore
    if text.is_empty() {
        return Ok(MessageAction::Ignore);
    }

    // Normal message - query Claude
    Ok(MessageAction::QueryClaude {
        text: text.to_string(),
    })
}

/// Build a message combining text and image paths.
///
/// Images are referenced using @path syntax which Claude Code understands.
pub fn build_text_with_images(text: &str, image_paths: &[PathBuf]) -> String {
    let mut result = text.to_string();

    for (i, path) in image_paths.iter().enumerate() {
        if let Some(path_str) = path.to_str() {
            if result.is_empty() {
                result = format!("@{}", path_str);
            } else if i == 0 {
                result = format!("{}\n\n@{}", result, path_str);
            } else {
                result = format!("{} @{}", result, path_str);
            }
        }
    }

    result
}

/// Execute an action that doesn't require the task manager.
///
/// Returns `Some(text)` if the action is QueryClaude (needs task_manager handling),
/// otherwise executes the action and returns `None`.
pub async fn execute_action(
    channel: &dyn Channel,
    user_id: &str,
    action: MessageAction,
) -> Result<Option<String>> {
    match action {
        MessageAction::SendResponse(response) => {
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::NeedsPairing { code } => {
            let response = format!(
                "Hi! I don't recognize you yet.\n\n\
                 Pairing code: {}\n\n\
                 Ask the owner to run:\n\
                 cica approve {}",
                code, code
            );
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::ExecuteCronJob { job_id } => {
            channel.send_message("Running job...").await?;
            let _typing = channel.start_typing();
            let result = execute_cron_job(&job_id, channel.name(), user_id).await;
            let response = result.unwrap_or_else(|e| format!("Job failed: {}", e));
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::Onboarding { message } => {
            let _typing = channel.start_typing();
            let response = handle_onboarding(channel.name(), user_id, &message).await?;
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::QueryClaude { text } => {
            // Return the text so caller can handle with task_manager
            Ok(Some(text))
        }

        MessageAction::Ignore => Ok(None),
    }
}

/// Execute a Claude query for the user.
///
/// This is called from within the task_manager callback after messages
/// have been debounced and batched.
pub async fn execute_claude_query(channel: Arc<dyn Channel>, user_id: &str, messages: Vec<String>) {
    let combined_text = messages.join("\n\n");
    let _typing = channel.start_typing();

    // Build context prompt
    let context_prompt = match onboarding::build_context_prompt_for_user(
        Some(channel.display_name()),
        Some(channel.name()),
        Some(user_id),
        Some(&combined_text),
    ) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to build context prompt: {}", e);
            let _ = channel
                .send_message(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    // Load pairing store for session management
    let mut store = match PairingStore::load() {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to load pairing store: {}", e);
            let _ = channel
                .send_message(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    // Query Claude with session
    let (response, _session_id) = match query_claude_with_session(
        &mut store,
        channel.name(),
        user_id,
        &combined_text,
        context_prompt,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Claude query failed: {}", e);
            let _ = channel
                .send_message(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    // Send response
    if let Err(e) = channel.send_message(&response).await {
        warn!("Failed to send message: {}", e);
    }

    // Re-index memories in case Claude saved new ones
    reindex_user_memories(channel.name(), user_id);
}

// ============================================================================
// Task Manager
// ============================================================================

/// Debounce duration for batching rapid messages
const DEBOUNCE_MS: u64 = 200;

/// Active task for a user
struct ActiveTask {
    handle: JoinHandle<()>,
}

/// Manages per-user message processing with debouncing and interruption
pub struct UserTaskManager {
    tasks: Mutex<HashMap<String, ActiveTask>>,
    pending: Mutex<HashMap<String, Vec<String>>>,
}

impl UserTaskManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Process a message for a user.
    /// If there's already a task running for this user, it will be aborted.
    /// Messages are debounced - if more arrive within DEBOUNCE_MS, they're batched.
    pub async fn process_message<F, Fut>(
        self: &Arc<Self>,
        user_key: String,
        message: String,
        handler: F,
    ) where
        F: FnOnce(Vec<String>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        debug!("Queueing message for {}: {}", user_key, message);

        // Add message to pending queue
        {
            let mut pending = self.pending.lock().await;
            pending
                .entry(user_key.clone())
                .or_insert_with(Vec::new)
                .push(message);
        }

        let mut tasks = self.tasks.lock().await;

        // If there's an existing task, abort it - we'll start fresh with all pending messages
        if let Some(existing) = tasks.remove(&user_key) {
            debug!("Aborting existing task for {}", user_key);
            existing.handle.abort();
        }

        // Spawn new task with debounce
        let manager = Arc::clone(self);
        let user_key_clone = user_key.clone();

        let handle = tokio::spawn(async move {
            // Debounce: wait a bit for more messages
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;

            // Collect all pending messages for this user
            let messages = {
                let mut pending = manager.pending.lock().await;
                pending.remove(&user_key_clone).unwrap_or_default()
            };

            if messages.is_empty() {
                return;
            }

            debug!(
                "Processing {} message(s) for {}",
                messages.len(),
                user_key_clone
            );

            // Run the handler
            handler(messages).await;

            // Clean up task entry
            manager.tasks.lock().await.remove(&user_key_clone);
        });

        tasks.insert(user_key, ActiveTask { handle });
    }
}

/// Result of processing a command
pub enum CommandResult {
    /// Not a command, continue with normal message processing
    NotACommand,
    /// Command was handled, return this response to the user
    Response(String),
    /// Trigger async cron job execution (job_id)
    CronRun(String),
}

/// Available commands
const COMMANDS: &[(&str, &str)] = &[
    ("/commands", "Show available commands"),
    ("/new", "Start a new conversation"),
    ("/skills", "List available skills"),
    ("/cron", "Manage scheduled jobs"),
];

/// Process a command if the message is one.
pub fn process_command(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
    onboarding_complete: bool,
) -> Result<CommandResult> {
    let text = text.trim();

    if text == "/commands" {
        let mut response = String::from("Available commands:\n");
        for (cmd, desc) in COMMANDS {
            response.push_str(&format!("\n{} - {}", cmd, desc));
        }
        return Ok(CommandResult::Response(response));
    }

    if text == "/new" {
        if !onboarding_complete {
            return Ok(CommandResult::Response(
                "Please complete the onboarding first. Say \"hello\" to get started!".to_string(),
            ));
        }
        let session_key = format!("{}:{}", channel, user_id);
        store.sessions.remove(&session_key);
        store.save()?;
        return Ok(CommandResult::Response(
            "Starting fresh! Our previous conversation has been cleared.".to_string(),
        ));
    }

    if text == "/skills" {
        let available_skills = skills::discover_skills().unwrap_or_default();
        if available_skills.is_empty() {
            return Ok(CommandResult::Response("No skills installed.".to_string()));
        }
        let mut response = String::from("Available skills:\n");
        for skill in available_skills {
            response.push_str(&format!("\n• {} - {}", skill.name, skill.description));
        }
        return Ok(CommandResult::Response(response));
    }

    // Handle /cron commands
    if text.starts_with("/cron") {
        let args = text.strip_prefix("/cron").unwrap_or("").trim();
        return process_cron_command(channel, user_id, args);
    }

    Ok(CommandResult::NotACommand)
}

/// Process /cron subcommands
fn process_cron_command(channel: &str, user_id: &str, args: &str) -> Result<CommandResult> {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let subcommand = parts.first().copied().unwrap_or("help");
    let rest = parts.get(1).copied().unwrap_or("");

    match subcommand {
        "list" | "ls" => {
            let store = CronStore::load()?;
            let jobs = store.list_for_user(channel, user_id);

            if jobs.is_empty() {
                return Ok(CommandResult::Response(
                    "No scheduled jobs.\n\nUse /cron add to create one. Try /cron help for usage."
                        .to_string(),
                ));
            }

            let mut response = String::from("Your scheduled jobs:\n");
            for job in jobs {
                let status = job.state.last_status.as_str();
                let next = job
                    .state
                    .next_run_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "—".to_string());
                let enabled = if job.enabled { "" } else { " (paused)" };

                response.push_str(&format!(
                    "\n[{}] {}{}\n  Schedule: {}\n  Status: {} | Next: {}\n",
                    job.short_id(),
                    job.name,
                    enabled,
                    job.schedule.description(),
                    status,
                    next
                ));
            }
            Ok(CommandResult::Response(response))
        }

        "add" => {
            if rest.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron add <schedule> <prompt>\n\n\
                     Examples:\n\
                     /cron add every 1h Check my emails\n\
                     /cron add every 10s Say hello\n\
                     /cron add 0 9 * * * Good morning!"
                        .to_string(),
                ));
            }

            let (schedule, prompt) = match parse_add_command(rest) {
                Ok(result) => result,
                Err(e) => return Ok(CommandResult::Response(format!("Error: {}", e))),
            };

            let name = truncate_for_name(&prompt, 30);
            let mut store = CronStore::load()?;
            let job = cron::CronJob::new(
                name.clone(),
                prompt,
                schedule.clone(),
                channel.to_string(),
                user_id.to_string(),
            );
            let id = store.add(job)?;

            let next = match &schedule {
                CronSchedule::At(ts) => format_timestamp(*ts),
                CronSchedule::Every(_) | CronSchedule::Cron(_) => {
                    let store = CronStore::load()?;
                    store
                        .jobs
                        .get(&id)
                        .and_then(|j| j.state.next_run_at)
                        .map(format_timestamp)
                        .unwrap_or_else(|| "soon".to_string())
                }
            };

            Ok(CommandResult::Response(format!(
                "Created job [{}] \"{}\"\nSchedule: {}\nNext run: {}\n\nUse /cron run {} to test it now!",
                &id[..8],
                name,
                schedule.description(),
                next,
                &id[..8]
            )))
        }

        "remove" | "rm" | "delete" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron remove <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;

            // Find job by full ID or prefix
            let job_id = find_job_id(&store, channel, user_id, id)?;

            match store.remove(&job_id, channel, user_id)? {
                Some(job) => Ok(CommandResult::Response(format!(
                    "Removed job [{}] \"{}\"",
                    job.short_id(),
                    job.name
                ))),
                None => Ok(CommandResult::Response(format!("Job not found: {}", id))),
            }
        }

        "run" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron run <job-id>".to_string(),
                ));
            }

            let store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;

            // Return special variant for async execution by the channel handler
            Ok(CommandResult::CronRun(job_id))
        }

        "pause" | "disable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron pause <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;

            let result = if let Some(job) = store.get_mut(&job_id) {
                if job.channel != channel || job.user_id != user_id {
                    return Ok(CommandResult::Response("Job not found".to_string()));
                }
                job.enabled = false;
                job.state.next_run_at = None;
                Some((job.short_id().to_string(), job.name.clone()))
            } else {
                None
            };

            if let Some((short_id, name)) = result {
                store.save()?;
                Ok(CommandResult::Response(format!(
                    "Paused job [{}] \"{}\"",
                    short_id, name
                )))
            } else {
                Ok(CommandResult::Response(format!("Job not found: {}", id)))
            }
        }

        "resume" | "enable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron resume <job-id>".to_string(),
                ));
            }

            let mut store = CronStore::load()?;
            let job_id = find_job_id(&store, channel, user_id, id)?;

            let result = if let Some(job) = store.get_mut(&job_id) {
                if job.channel != channel || job.user_id != user_id {
                    return Ok(CommandResult::Response("Job not found".to_string()));
                }
                job.enabled = true;
                job.update_next_run(cron::store::now_millis());
                let next = job
                    .state
                    .next_run_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "soon".to_string());
                Some((job.short_id().to_string(), job.name.clone(), next))
            } else {
                None
            };

            if let Some((short_id, name, next)) = result {
                store.save()?;
                Ok(CommandResult::Response(format!(
                    "Resumed job [{}] \"{}\"\nNext run: {}",
                    short_id, name, next
                )))
            } else {
                Ok(CommandResult::Response(format!("Job not found: {}", id)))
            }
        }

        _ => Ok(CommandResult::Response(
            "Cron job commands:\n\n\
             /cron list - List your scheduled jobs\n\
             /cron add <schedule> <prompt> - Create a new job\n\
             /cron remove <job-id> - Delete a job\n\
             /cron run <job-id> - Run immediately (for testing)\n\
             /cron pause <job-id> - Pause a job\n\
             /cron resume <job-id> - Resume a paused job\n\n\
             Schedule formats:\n\
             • every 10s / every 5m / every 1h - Recurring interval\n\
             • at 2024-01-28 14:00 - One-time execution\n\
             • 0 9 * * * - Cron expression (9 AM daily)\n\n\
             Examples:\n\
             /cron add every 1h Check my inbox\n\
             /cron add every 10s Say hello\n\
             /cron add 0 9 * * * Good morning!"
                .to_string(),
        )),
    }
}

/// Execute a cron job manually and return the output.
/// Shared by all channel handlers.
pub async fn execute_cron_job(job_id: &str, channel: &str, user_id: &str) -> Result<String> {
    let store = CronStore::load()?;
    let job = store
        .get(job_id, channel, user_id)
        .ok_or_else(|| anyhow::anyhow!("Job not found"))?;

    let (response, _session_id) = claude::query_with_options(
        &job.prompt,
        QueryOptions {
            skip_permissions: true,
            ..Default::default()
        },
    )
    .await?;

    Ok(format!("[Cron: {}]\n\n{}", job.name, response))
}

/// Find a job ID by full ID or prefix match
fn find_job_id(
    store: &CronStore,
    channel: &str,
    user_id: &str,
    id_or_prefix: &str,
) -> Result<String> {
    let id = id_or_prefix.trim();

    // First try exact match
    if store.get(id, channel, user_id).is_some() {
        return Ok(id.to_string());
    }

    // Try prefix match
    let matches: Vec<_> = store
        .list_for_user(channel, user_id)
        .into_iter()
        .filter(|j| j.id.starts_with(id))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("Job not found: {}", id),
        1 => Ok(matches[0].id.clone()),
        _ => anyhow::bail!(
            "Ambiguous job ID '{}'. Matches: {}",
            id,
            matches
                .iter()
                .map(|j| j.short_id())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Query Claude with automatic session recovery.
///
/// If the session has expired, clears it and retries with a fresh conversation.
/// Returns the response text and the new session ID.
pub async fn query_claude_with_session(
    store: &mut PairingStore,
    channel: &str,
    user_id: &str,
    text: &str,
    context_prompt: String,
) -> Result<(String, String)> {
    let session_key = format!("{}:{}", channel, user_id);
    let existing_session = store.sessions.get(&session_key).cloned();

    let options = claude::QueryOptions {
        system_prompt: Some(context_prompt.clone()),
        resume_session: existing_session,
        skip_permissions: true,
        ..Default::default()
    };

    let (response, session_id) = match claude::query_with_options(text, options).await {
        Ok((response, session_id)) => (response, session_id),
        Err(e) => {
            let error_msg = e.to_string();
            // If session not found, clear it and retry without resuming
            if error_msg.contains("No conversation found with session ID") {
                warn!("Session expired, starting fresh conversation");
                store.sessions.remove(&session_key);
                store.save()?;

                let retry_options = claude::QueryOptions {
                    system_prompt: Some(context_prompt),
                    resume_session: None,
                    skip_permissions: true,
                    ..Default::default()
                };

                match claude::query_with_options(text, retry_options).await {
                    Ok((response, session_id)) => (response, session_id),
                    Err(e) => {
                        warn!("Claude error on retry: {}", e);
                        (
                            format!("Sorry, I encountered an error: {}", e),
                            String::new(),
                        )
                    }
                }
            } else {
                warn!("Claude error: {}", e);
                (
                    format!("Sorry, I encountered an error: {}", e),
                    String::new(),
                )
            }
        }
    };

    // Save session ID for future messages
    if !session_id.is_empty()
        && store.sessions.get(&session_key).map(|s| s.as_str()) != Some(&session_id)
    {
        store.sessions.insert(session_key, session_id.clone());
        store.save()?;
    }

    Ok((response, session_id))
}

/// Handle onboarding flow - Claude drives the conversation
pub async fn handle_onboarding(channel: &str, user_id: &str, message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt_for_user(channel, user_id)?;

    let options = claude::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true,
        ..Default::default()
    };

    let (response, _) = claude::query_with_options(message, options).await?;
    Ok(response)
}

/// Re-index memories for a user (called after Claude responds)
pub fn reindex_user_memories(channel: &str, user_id: &str) {
    match MemoryIndex::open() {
        Ok(mut index) => {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to re-index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
        }
    }
}

/// Information about a channel for display purposes
pub struct ChannelInfo {
    pub name: &'static str,
    pub display_name: &'static str,
}

/// List of all supported channels
pub const SUPPORTED_CHANNELS: &[ChannelInfo] = &[
    ChannelInfo {
        name: "telegram",
        display_name: "Telegram",
    },
    ChannelInfo {
        name: "signal",
        display_name: "Signal",
    },
];

/// Get channel info by name
pub fn get_channel_info(name: &str) -> Option<&'static ChannelInfo> {
    SUPPORTED_CHANNELS.iter().find(|c| c.name == name)
}
