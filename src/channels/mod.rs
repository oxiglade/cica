pub mod signal;
pub mod telegram;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::claude;
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::pairing::PairingStore;
use crate::skills;

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
}

/// Available commands
const COMMANDS: &[(&str, &str)] = &[
    ("/commands", "Show available commands"),
    ("/new", "Start a new conversation"),
    ("/skills", "List available skills"),
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

    Ok(CommandResult::NotACommand)
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
