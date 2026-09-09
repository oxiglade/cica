pub mod linear;
pub mod signal;
pub mod slack;
pub mod telegram;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::audit;
use crate::backends::{self, QueryResult};
use crate::cron::{
    CronSchedule, CronService, DeliveryTarget, SystemClock, format_timestamp, parse_add_command,
    truncate_for_name,
};
use crate::memory::MemoryIndex;
use crate::onboarding;
use crate::runtime::{Runtime, lock};
use crate::sandbox::{self, Affinity, SessionPersistence, TurnJob};
use crate::skills;

pub type SessionLocks = std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// Abstraction over channel-specific transport operations.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Channel identifier (e.g., "telegram", "signal")
    fn name(&self) -> &'static str;

    /// Display name for user-facing messages (e.g., "Telegram", "Signal")
    fn display_name(&self) -> &'static str;

    /// Send a text message to the user
    async fn send_message(&self, message: &str) -> Result<()>;

    /// Report a failed turn; transports without a distinct error message default to `send_message`.
    async fn send_error(&self, message: &str) -> Result<()> {
        self.send_message(message).await
    }

    /// Send a message with attachments (images, files, etc.)
    async fn send_message_with_attachments(
        &self,
        message: &str,
        _attachment_paths: &[PathBuf],
    ) -> Result<()> {
        self.send_message(message).await
    }

    /// Start a typing indicator. Returns a guard that stops the indicator when dropped.
    fn start_typing(&self) -> TypingGuard;
}

/// Keys memories, `USER.md`, and pairing by `<channel>_<user_id>`, potentially on another channel; `Channel` separately identifies the conversation's display surface.
#[derive(Debug, Clone)]
pub struct Identity {
    pub channel: String,
    pub user_id: String,
}

impl Identity {
    /// The default: attribute the turn to the channel it arrived on.
    pub fn of(channel: &dyn Channel, user_id: &str) -> Self {
        Self {
            channel: channel.name().to_string(),
            user_id: user_id.to_string(),
        }
    }

    /// Attribute the turn to an identity on another channel.
    pub fn mapped(channel: String, user_id: String) -> Self {
        Self { channel, user_id }
    }
}

/// RAII guard for typing indicators; dropped when the response is ready.
pub struct TypingGuard {
    cancel: Option<oneshot::Sender<()>>,
}

impl TypingGuard {
    pub fn new(cancel: oneshot::Sender<()>) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

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

/// Actions that can result from processing an incoming message.
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
#[allow(clippy::too_many_arguments)]
pub fn determine_action(
    rt: &Runtime,
    channel: &str,
    user_id: &str,
    text: &str,
    _image_paths: &[PathBuf],
    username: Option<String>,
    display_name: Option<String>,
    session_key_override: Option<&str>,
) -> Result<MessageAction> {
    let text = text.trim();

    let mut store = lock(&rt.pairing);
    if !store.is_approved(channel, user_id) {
        store.reload()?;
        if !store.is_approved(channel, user_id) {
            let settings = rt.config.channel_settings(channel);

            if settings.auto_approve {
                store
                    .modify(|store| store.auto_approve(channel, user_id, username, display_name))?;
            } else {
                let (code, _is_new) = store.modify(|store| {
                    store.get_or_create_pending(channel, user_id, username, display_name)
                })?;
                return Ok(MessageAction::NeedsPairing { code });
            }
        }
    }
    drop(store);

    let settings = rt.config.channel_settings(channel);
    let onboarding_complete =
        onboarding::is_complete_for_user(&rt.paths, &settings, channel, user_id)?;

    // Commands work even during onboarding.
    match process_command(
        rt,
        channel,
        user_id,
        text,
        onboarding_complete,
        session_key_override,
    )? {
        CommandResult::Response(response) => {
            return Ok(MessageAction::SendResponse(response));
        }
        CommandResult::CronRun(job_id) => {
            return Ok(MessageAction::ExecuteCronJob { job_id });
        }
        CommandResult::NotACommand => {}
    }

    if !onboarding_complete {
        let message = if text == "/start" { "hi" } else { text };
        return Ok(MessageAction::Onboarding {
            message: message.to_string(),
        });
    }

    if text == "/start" {
        return Ok(MessageAction::Ignore);
    }

    if text.is_empty() {
        return Ok(MessageAction::Ignore);
    }

    Ok(MessageAction::QueryClaude {
        text: text.to_string(),
    })
}

/// Returns the prompt text plus the attachment paths it references.
pub fn build_text_with_images(
    base: &Path,
    text: &str,
    image_paths: &[PathBuf],
) -> (String, Vec<String>) {
    let mut result = text.to_string();
    let mut attachments = Vec::new();

    for (i, path) in image_paths.iter().enumerate() {
        let relative = path.strip_prefix(base).ok().and_then(Path::to_str);
        if let Some(path_str) = relative.or_else(|| path.to_str()) {
            if result.is_empty() {
                result = format!("@{}", path_str);
            } else if i == 0 {
                result = format!("{}\n\n@{}", result, path_str);
            } else {
                result = format!("{} @{}", result, path_str);
            }
            if let Some(relative) = relative {
                attachments.push(relative.to_string());
            }
        }
    }

    (result, attachments)
}

#[cfg(test)]
pub(crate) fn assert_prompt_paths_resolve(
    base: &Path,
    (prompt, attachments): &(String, Vec<String>),
) {
    for path in attachments {
        assert!(
            base.join(path).is_file(),
            "attachment path does not resolve to a file: {path}"
        );
        assert!(
            prompt.contains(&format!("@{path}")),
            "prompt does not reference attachment path: {path}"
        );
    }
}

/// Execute an action. Returns `Some(text)` for `QueryClaude` (caller handles with task manager).
pub async fn execute_action(
    rt: &Runtime,
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
            let result = execute_cron_job(rt, &job_id, channel.name(), user_id).await;
            let response = result.unwrap_or_else(|e| format!("Job failed: {}", e));
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::Onboarding { message } => {
            let _typing = channel.start_typing();
            let response = handle_onboarding(rt, channel.name(), user_id, &message).await?;
            channel.send_message(&response).await?;
            Ok(None)
        }

        MessageAction::QueryClaude { text } => Ok(Some(text)),

        MessageAction::Ignore => Ok(None),
    }
}

/// Extract media file paths from Claude's response text.
///
/// Prefers explicit `[attachment:/path/to/file]` markers; falls back to heuristic
/// path detection for backwards compatibility.
fn extract_media_attachments(response: &str) -> Vec<PathBuf> {
    let mut attachments = Vec::new();

    for marker in crate::sandbox::attachment_markers(response) {
        let path = PathBuf::from(marker);
        if path.exists() && !attachments.contains(&path) {
            attachments.push(path);
        }
    }

    if !attachments.is_empty() {
        return attachments;
    }

    // Fallback: heuristic detection for paths ending in media extensions.
    let media_extensions = [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".mp4", ".mov", ".webm", ".avi",
    ];

    for line in response.lines() {
        let line = line.trim();

        for ext in &media_extensions {
            if line.contains(ext)
                && let Some(start) = line.find("/Users/")
                && let Some(ext_pos) = line[start..].find(ext)
            {
                let end_pos = start + ext_pos + ext.len();
                let path_str = &line[start..end_pos];
                if std::path::Path::new(path_str).exists() {
                    attachments.push(PathBuf::from(path_str));
                    break;
                }
            }
        }
    }

    attachments
}

/// Remove lines with file paths or attachment markers before sending to the user.
fn remove_file_path_lines(response: &str) -> String {
    let lines: Vec<&str> = response
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            let lower = trimmed.to_lowercase();
            !trimmed.contains("[attachment:")
                && !trimmed.contains("/Users/")
                && !lower.contains("saved to")
                && !lower.contains("image has been saved")
                && !lower.contains("video has been saved")
                && !lower.contains("file has been saved")
                && !trimmed.is_empty()
        })
        .collect();

    lines.join("\n").trim().to_string()
}

/// Execute a Claude query for the user (called from the task_manager callback).
pub async fn execute_claude_query(
    rt: Arc<Runtime>,
    channel: Arc<dyn Channel>,
    identity: &Identity,
    affinity: Affinity,
    messages: Vec<String>,
    session_key: Option<String>,
    attachments: Vec<String>,
) {
    let combined_text = messages.join("\n\n");
    let _typing = channel.start_typing();

    let user_id = identity.user_id.as_str();

    let context_prompt = match onboarding::build_context_prompt_for_user(
        &rt.config,
        &rt.paths,
        Some(channel.display_name()),
        Some(&identity.channel),
        Some(user_id),
        Some(&combined_text),
    ) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to build context prompt: {}", e);
            let _ = channel
                .send_error(&format!("Sorry, I encountered an error: {}", e))
                .await;
            return;
        }
    };

    let qr = match query_ai_with_session(
        &rt,
        &identity.channel,
        user_id,
        affinity,
        &combined_text,
        context_prompt,
        session_key.as_deref(),
        attachments,
    )
    .await
    {
        Ok(qr) => qr,
        Err(e) => {
            warn!("AI query failed: {}", e);
            let err_msg = format!("Sorry, I encountered an error: {}", e);
            audit::log_message(
                channel.name(),
                user_id,
                &combined_text,
                &err_msg,
                None,
                None,
                None,
                true,
            );
            let _ = channel.send_error(&err_msg).await;
            return;
        }
    };

    let response = &qr.response;

    audit::log_message(
        channel.name(),
        user_id,
        &combined_text,
        response,
        if qr.session_id.is_empty() {
            None
        } else {
            Some(qr.session_id.as_str())
        },
        qr.duration_ms,
        qr.cost_usd,
        false,
    );

    deliver(
        channel.as_ref(),
        response,
        &sandbox::outbound_dir(&rt.paths.base),
    )
    .await;

    reindex_user_memories(&rt, channel.name(), user_id).await;
}

async fn deliver(channel: &dyn Channel, response: &str, outbound: &Path) {
    let attachments = extract_media_attachments(response);
    if attachments.is_empty() {
        if let Err(e) = channel.send_message(response).await {
            warn!("Failed to send message: {}", e);
        }
        return;
    }

    debug!("Sending response with {} attachment(s)", attachments.len());
    let cleaned_response = remove_file_path_lines(response);
    if let Err(e) = channel
        .send_message_with_attachments(&cleaned_response, &attachments)
        .await
    {
        warn!(
            "Failed to send message with attachments, falling back to text: {}",
            e
        );
        let fallback =
            format!("{cleaned_response}\n\n(I produced a file for this but could not attach it.)");
        if let Err(e) = channel.send_message(&fallback).await {
            warn!("Failed to send the fallback message: {}", e);
        }
    }
    discard_produced(&attachments, outbound);
}

/// The router's copy of a produced file exists only to be sent; the store copy
/// went with the turn. Removes the per-turn directory under `outbound`, and
/// leaves any other path (an inbound attachment the agent echoed) alone.
fn discard_produced(attachments: &[PathBuf], outbound: &Path) {
    for path in attachments {
        let Some(turn_dir) = path.parent() else {
            continue;
        };
        if turn_dir.parent() != Some(outbound) {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(turn_dir) {
            warn!(
                "Failed to remove produced files {}: {}",
                turn_dir.display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    struct Recording {
        fail_attachments: bool,
        sent: std::sync::Mutex<Vec<(String, usize)>>,
    }

    #[async_trait]
    impl Channel for Recording {
        fn name(&self) -> &'static str {
            "test"
        }
        fn display_name(&self) -> &'static str {
            "Test"
        }
        async fn send_message(&self, message: &str) -> Result<()> {
            self.sent.lock().unwrap().push((message.to_string(), 0));
            Ok(())
        }
        async fn send_message_with_attachments(
            &self,
            message: &str,
            paths: &[PathBuf],
        ) -> Result<()> {
            if self.fail_attachments {
                anyhow::bail!("missing_scope");
            }
            self.sent
                .lock()
                .unwrap()
                .push((message.to_string(), paths.len()));
            Ok(())
        }
        fn start_typing(&self) -> TypingGuard {
            TypingGuard::noop()
        }
    }

    fn response_naming_a_file(dir: &Path) -> String {
        let file = dir.join("export.json");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&file, b"{}").unwrap();
        format!("Here it is.\n[attachment:{}]", file.display())
    }

    #[tokio::test]
    async fn a_failed_upload_still_delivers_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let channel = Recording {
            fail_attachments: true,
            sent: Default::default(),
        };
        let outbound = dir.path().join("outbound");
        deliver(
            &channel,
            &response_naming_a_file(&outbound.join("turn-1")),
            &outbound,
        )
        .await;

        assert!(
            !outbound.join("turn-1").exists(),
            "the router copy outlived the send"
        );
        let sent = channel.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "expected exactly one fallback message");
        let (text, attachments) = &sent[0];
        assert_eq!(*attachments, 0);
        assert!(text.starts_with("Here it is."));
        assert!(text.contains("could not attach"));
        assert!(!text.contains("[attachment:"));
    }

    #[tokio::test]
    async fn a_successful_upload_sends_once_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let channel = Recording {
            fail_attachments: false,
            sent: Default::default(),
        };
        let outbound = dir.path().join("outbound");
        deliver(
            &channel,
            &response_naming_a_file(&outbound.join("turn-1")),
            &outbound,
        )
        .await;

        assert!(!outbound.join("turn-1").exists());
        let sent = channel.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, 1);
        assert!(!sent[0].0.contains("could not attach"));
    }

    #[tokio::test]
    async fn a_file_outside_the_outbound_dir_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let channel = Recording {
            fail_attachments: false,
            sent: Default::default(),
        };
        let inbound = dir.path().join("inbound");
        deliver(
            &channel,
            &response_naming_a_file(&inbound),
            &dir.path().join("outbound"),
        )
        .await;

        assert!(inbound.join("export.json").is_file());
        assert_eq!(channel.sent.lock().unwrap()[0].1, 1);
    }
}

const DEBOUNCE_MS: u64 = 200;

struct ActiveTask {
    handle: JoinHandle<()>,
    generation: u64,
}

/// Manages per-user message processing with debouncing and interruption
pub struct UserTaskManager {
    tasks: Mutex<HashMap<String, ActiveTask>>,
    pending: Mutex<HashMap<String, Vec<String>>>,
    next_generation: AtomicU64,
}

impl UserTaskManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(0),
        })
    }

    /// Queue a message for processing; aborts any in-flight task and batches within DEBOUNCE_MS.
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

        {
            let mut pending = self.pending.lock().await;
            pending
                .entry(user_key.clone())
                .or_insert_with(Vec::new)
                .push(message);
        }

        let mut tasks = self.tasks.lock().await;

        if let Some(existing) = tasks.remove(&user_key) {
            debug!("Aborting existing task for {}", user_key);
            existing.handle.abort();
        }

        let manager = Arc::clone(self);
        let user_key_clone = user_key.clone();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;

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

            handler(messages).await;

            manager
                .cleanup_generation(&user_key_clone, generation)
                .await;
        });

        tasks.insert(user_key, ActiveTask { handle, generation });
    }

    async fn cleanup_generation(&self, user_key: &str, generation: u64) {
        let mut tasks = self.tasks.lock().await;
        if tasks
            .get(user_key)
            .is_some_and(|task| task.generation == generation)
        {
            tasks.remove(user_key);
        }
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
    ("/usage", "Show your usage stats"),
];

pub fn process_command(
    rt: &Runtime,
    channel: &str,
    user_id: &str,
    text: &str,
    onboarding_complete: bool,
    session_key_override: Option<&str>,
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
        let session_key = session_key_override
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{}", channel, user_id));
        let old_session_id =
            lock(&rt.pairing).modify(|store| Ok(store.sessions.remove(&session_key)))?;

        let detail = old_session_id
            .as_ref()
            .map(|sid| format!("{{\"old_session_id\":\"{}\"}}", sid));
        audit::log_event(
            "session_reset",
            Some(channel),
            Some(user_id),
            detail.as_deref(),
        );
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/new\"}"),
        );

        return Ok(CommandResult::Response(
            "Starting fresh! Our previous conversation has been cleared.".to_string(),
        ));
    }

    if text == "/usage" {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/usage\"}"),
        );
        let response = match audit::get_usage(channel, user_id) {
            Ok((count, total_cost)) => {
                let cost_line = match total_cost {
                    Some(cost) if cost > 0.0 => format!("Total cost: ${:.4}\n", cost),
                    _ => String::new(),
                };
                format!("Your usage:\n\nMessages: {}\n{}", count, cost_line)
            }
            Err(_) => "Usage stats not available.".to_string(),
        };
        return Ok(CommandResult::Response(response));
    }

    if text == "/skills" {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some("{\"command\":\"/skills\"}"),
        );
        let available_skills = skills::discover_skills(
            &rt.paths,
            crate::config::prep_skill_deps_locally(rt.config.deployment.provider),
        )
        .unwrap_or_default();
        if available_skills.is_empty() {
            return Ok(CommandResult::Response("No skills installed.".to_string()));
        }
        let mut response = String::from("Available skills:\n");
        for skill in available_skills {
            response.push_str(&format!("\n• {} - {}", skill.name, skill.description));
        }
        return Ok(CommandResult::Response(response));
    }

    if text.starts_with("/cron") {
        audit::log_event(
            "command_used",
            Some(channel),
            Some(user_id),
            Some(&format!(
                "{{\"command\":\"{}\"}}",
                text.split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        );
        let args = text.strip_prefix("/cron").unwrap_or("").trim();
        return process_cron_command(&rt.cron, channel, user_id, args);
    }

    Ok(CommandResult::NotACommand)
}

/// Extract --target <value> from a command string, returning (target, remaining_text).
fn extract_target_flag(input: &str) -> (Option<DeliveryTarget>, String) {
    if let Some(idx) = input.find("--target ") {
        let after_flag = &input[idx + "--target ".len()..];
        let value_end = after_flag.find(' ').unwrap_or(after_flag.len());
        let target_value = &after_flag[..value_end];

        let before = input[..idx].trim();
        let after = if value_end < after_flag.len() {
            after_flag[value_end..].trim()
        } else {
            ""
        };
        let remaining = format!("{} {}", before, after).trim().to_string();

        let target = DeliveryTarget::channel(target_value.to_string());
        (Some(target), remaining)
    } else {
        (None, input.to_string())
    }
}

fn process_cron_command(
    cron: &CronService<SystemClock>,
    channel: &str,
    user_id: &str,
    args: &str,
) -> Result<CommandResult> {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let subcommand = parts.first().copied().unwrap_or("help");
    let rest = parts.get(1).copied().unwrap_or("");

    match subcommand {
        "list" | "ls" => {
            let jobs = cron.list(channel, user_id);

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
                let target_info = if job.target.channel_id.is_some() {
                    format!(
                        "  Target: {}{}\n",
                        job.target.channel_id.as_deref().unwrap_or("DM"),
                        job.target
                            .thread_id
                            .as_ref()
                            .map(|t| format!(" (thread: {})", t))
                            .unwrap_or_default()
                    )
                } else {
                    String::new()
                };

                response.push_str(&format!(
                    "\n[{}] {}{}\n  Schedule: {}\n{}  Status: {} | Next: {}\n",
                    job.short_id(),
                    job.name,
                    enabled,
                    job.schedule.description(),
                    target_info,
                    status,
                    next
                ));
            }
            Ok(CommandResult::Response(response))
        }

        "add" => {
            if rest.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron add <schedule> <prompt> [--target <channel_id>]\n\n\
                     Examples:\n\
                     /cron add every 1h Check my emails\n\
                     /cron add every 10s Say hello\n\
                     /cron add 0 9 * * * Good morning!\n\
                     /cron add every 1h Check emails --target C0123456789"
                        .to_string(),
                ));
            }

            let (target, rest_without_target) = extract_target_flag(rest);

            let (schedule, prompt) = match parse_add_command(&rest_without_target) {
                Ok(result) => result,
                Err(e) => return Ok(CommandResult::Response(format!("Error: {}", e))),
            };

            let name = truncate_for_name(&prompt, 30);
            let job = cron.add(
                name.clone(),
                prompt,
                schedule.clone(),
                channel.to_string(),
                user_id.to_string(),
                target,
            )?;
            let id = &job.id;

            let next = match &schedule {
                CronSchedule::At(ts) => format_timestamp(*ts),
                CronSchedule::Every(_) | CronSchedule::Cron(_) => job
                    .state
                    .next_run_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "soon".to_string()),
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

            let job_id = cron.resolve_id(channel, user_id, id)?;

            match cron.remove(&job_id, channel, user_id)? {
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

            let job_id = cron.resolve_id(channel, user_id, id)?;
            Ok(CommandResult::CronRun(job_id))
        }

        "pause" | "disable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron pause <job-id>".to_string(),
                ));
            }

            let job_id = cron.resolve_id(channel, user_id, id)?;
            let job = cron.set_enabled(&job_id, channel, user_id, false)?;
            Ok(CommandResult::Response(format!(
                "Paused job [{}] \"{}\"",
                job.short_id(),
                job.name
            )))
        }

        "resume" | "enable" => {
            let id = rest.trim();
            if id.is_empty() {
                return Ok(CommandResult::Response(
                    "Usage: /cron resume <job-id>".to_string(),
                ));
            }

            let job_id = cron.resolve_id(channel, user_id, id)?;
            let job = cron.set_enabled(&job_id, channel, user_id, true)?;
            let next = job
                .state
                .next_run_at
                .map(format_timestamp)
                .unwrap_or_else(|| "soon".to_string());
            Ok(CommandResult::Response(format!(
                "Resumed job [{}] \"{}\"\nNext run: {}",
                job.short_id(),
                job.name,
                next
            )))
        }

        _ => Ok(CommandResult::Response(
            "Cron job commands:\n\n\
             /cron list - List your scheduled jobs\n\
             /cron add <schedule> <prompt> [--target <channel_id>] - Create a new job\n\
             /cron remove <job-id> - Delete a job\n\
             /cron run <job-id> - Run immediately (for testing)\n\
             /cron pause <job-id> - Pause a job\n\
             /cron resume <job-id> - Resume a paused job\n\n\
             Schedule formats:\n\
             • every 10s / every 5m / every 1h - Recurring interval\n\
             • at 2024-01-28 14:00 - One-time execution\n\
             • 0 9 * * * - Cron expression (9 AM daily)\n\n\
             Options:\n\
             • --target <channel_id> - Send results to a specific channel (default: DM)\n\n\
             Examples:\n\
             /cron add every 1h Check my inbox\n\
             /cron add every 10s Say hello\n\
             /cron add 0 9 * * * Good morning!\n\
             /cron add every 1h Check emails --target C0123456789"
                .to_string(),
        )),
    }
}

/// Execute a cron job manually and return the output.
pub async fn execute_cron_job(
    rt: &Runtime,
    job_id: &str,
    channel: &str,
    user_id: &str,
) -> Result<String> {
    let job = rt
        .cron
        .status(job_id, channel, user_id)
        .ok_or_else(|| anyhow::anyhow!("Job not found"))?;

    let channel_display = get_channel_info(channel).map(|c| c.display_name);
    let context_prompt = onboarding::build_context_prompt_for_user(
        &rt.config,
        &rt.paths,
        channel_display,
        Some(channel),
        Some(user_id),
        Some(&job.prompt),
    )?;

    let turn = TurnJob::new(
        &rt.config,
        channel,
        user_id,
        Affinity::Cron {
            job_id: job.id.clone(),
        },
        job.prompt.clone(),
        Some(context_prompt),
        None,
    );
    let turn = TurnJob {
        session_persistence: SessionPersistence::None,
        ..turn
    };

    let tr = rt.provider.run_turn(turn).await?;

    Ok(format!("[Cron: {}]\n\n{}", job.name, tr.response))
}

/// Query the AI backend through the configured provider and persist the returned session id.
#[allow(clippy::too_many_arguments)]
pub async fn query_ai_with_session(
    rt: &Runtime,
    channel: &str,
    user_id: &str,
    affinity: Affinity,
    text: &str,
    context_prompt: String,
    session_key_override: Option<&str>,
    attachments: Vec<String>,
) -> Result<QueryResult> {
    let session_key = match session_key_override {
        Some(key) => key.to_string(),
        None => format!("{}:{}", channel, user_id),
    };
    let ticket = rt.session_ticket.fetch_add(1, Ordering::Relaxed);
    debug!(ticket, session_key, "waiting for session lock");
    let session_lock = {
        let mut locks = lock(&rt.session_locks);
        locks
            .entry(session_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    // Tokio's mutex serves waiters in FIFO order.
    let session_guard = session_lock.lock().await;
    debug!(ticket, session_key, "acquired session lock");
    let result = async {
        let existing_session = lock(&rt.pairing).sessions.get(&session_key).cloned();

        let job = TurnJob::new(
            &rt.config,
            channel,
            user_id,
            affinity,
            text.to_string(),
            Some(context_prompt),
            existing_session.clone(),
        )
        .with_attachments(attachments);

        let qr = sandbox::query_result_from_turn(rt.provider.run_turn(job).await?);

        if !qr.session_id.is_empty() {
            let written = lock(&rt.pairing).modify(|store| {
                Ok(store.set_session_if(&session_key, existing_session.as_deref(), &qr.session_id))
            })?;
            if !written {
                warn!(
                    "Session for {} changed during the turn; not overwriting",
                    session_key
                );
            }
        }

        Ok(qr)
    }
    .await;
    drop(session_guard);
    drop(session_lock);
    let mut locks = lock(&rt.session_locks);
    if locks
        .get(&session_key)
        .is_some_and(|entry| Arc::strong_count(entry) == 1)
    {
        locks.remove(&session_key);
    }
    result
}

/// Handle onboarding flow - AI drives the conversation
pub async fn handle_onboarding(
    rt: &Runtime,
    channel: &str,
    user_id: &str,
    message: &str,
) -> Result<String> {
    let settings = rt.config.channel_settings(channel);
    let system_prompt = onboarding::system_prompt_for_user(&rt.paths, &settings, channel, user_id)?;

    let options = backends::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true,
        ..Default::default()
    };

    let qr =
        backends::query_with_options(rt.config.backend, &rt.config, &rt.paths, message, options)
            .await?;
    Ok(qr.response)
}

/// Pull a user's memories from the state store into `dest`. `None` store
/// (single-box) is a no-op returning `Ok(false)` — never attempts a pull.
async fn pull_memories_with_store(
    store: Option<&std::sync::Arc<dyn crate::sandbox::state::StateStore>>,
    dest: &std::path::Path,
    channel: &str,
    user_id: &str,
) -> anyhow::Result<bool> {
    match store {
        Some(s) => s.pull(&format!("mem/{channel}_{user_id}"), dest).await,
        None => Ok(false),
    }
}

/// Pull the user's memories from the authoritative state store (cloud) or skip
/// (single-box), then re-index the local copy.
pub async fn reindex_user_memories(rt: &Runtime, channel: &str, user_id: &str) {
    // Cloud mode: S3 is authoritative for memories — pull this user's prefix so
    // the router index reflects what workers wrote. (Hand-edits on the router's
    // disk get clobbered by this pull; route operator edits through a turn or
    // straight to the store.) Single-box: no store → skipped. Best-effort.
    let dest = crate::memory::memories_dir(&rt.paths, channel, user_id);
    match crate::sandbox::state::default_store(&rt.config, &rt.paths) {
        Ok(store) => {
            if let Err(e) = pull_memories_with_store(store.as_ref(), &dest, channel, user_id).await
            {
                warn!(
                    "Failed to pull memories for {}:{} (reindexing local copy): {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => warn!(
            "Failed to resolve store for memory pull {}:{}: {}",
            channel, user_id, e
        ),
    }

    match MemoryIndex::open(&rt.paths) {
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
    ChannelInfo {
        name: "slack",
        display_name: "Slack",
    },
    ChannelInfo {
        name: "linear",
        display_name: "Linear",
    },
];

/// Get channel info by name
pub fn get_channel_info(name: &str) -> Option<&'static ChannelInfo> {
    SUPPORTED_CHANNELS.iter().find(|c| c.name == name)
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    struct FakeChannel;

    #[async_trait]
    impl Channel for FakeChannel {
        fn name(&self) -> &'static str {
            "slack"
        }
        fn display_name(&self) -> &'static str {
            "Slack"
        }
        async fn send_message(&self, _message: &str) -> Result<()> {
            Ok(())
        }
        fn start_typing(&self) -> TypingGuard {
            TypingGuard::noop()
        }
    }

    #[test]
    fn by_default_a_turn_is_attributed_to_the_channel_it_arrived_on() {
        let identity = Identity::of(&FakeChannel, "U1");
        assert_eq!(identity.channel, "slack");
        assert_eq!(identity.user_id, "U1");
    }

    #[test]
    fn a_mapped_identity_points_at_the_other_channels_memories() {
        let identity = Identity::mapped("slack".into(), "U0123ABC".into());
        assert_eq!(identity.channel, "slack");
        assert_eq!(identity.user_id, "U0123ABC");
    }

    #[test]
    fn a_mapped_identity_does_not_decide_which_surface_the_user_sees() {
        let identity = Identity::mapped("slack".into(), "U0123ABC".into());
        assert_eq!(identity.channel, "slack");
        assert_eq!(FakeChannel.display_name(), "Slack");
        let _: &str = &identity.channel;
    }
}

#[cfg(test)]
mod memory_pull_tests {
    use super::*;
    use crate::sandbox::state::{FilesystemStateStore, StateStore};
    use std::sync::Arc;

    #[tokio::test]
    async fn pulls_from_store_into_dest() {
        let store_root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));
        // Seed a memory blob at mem/telegram_1.
        let seed = tempfile::tempdir().unwrap();
        std::fs::write(seed.path().join("note.md"), "remember this").unwrap();
        store.push(seed.path(), "mem/telegram_1").await.unwrap();

        let store_dyn: Option<Arc<dyn StateStore>> = Some(store);
        let pulled = pull_memories_with_store(store_dyn.as_ref(), dest.path(), "telegram", "1")
            .await
            .unwrap();
        assert!(pulled);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("note.md")).unwrap(),
            "remember this"
        );
    }

    #[tokio::test]
    async fn no_store_is_a_noop() {
        let dest = tempfile::tempdir().unwrap();
        let pulled = pull_memories_with_store(None, dest.path(), "telegram", "1")
            .await
            .unwrap();
        assert!(!pulled);
        // dest stays empty — single-box must not attempt any pull.
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn absent_key_is_a_noop_and_does_not_clobber() {
        let store_root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));
        // Pre-existing local file that must survive a pull of a non-existent key.
        std::fs::write(dest.path().join("local.md"), "keep me").unwrap();

        let store_dyn: Option<Arc<dyn StateStore>> = Some(store);
        let pulled = pull_memories_with_store(store_dyn.as_ref(), dest.path(), "telegram", "1")
            .await
            .unwrap();
        assert!(!pulled);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("local.md")).unwrap(),
            "keep me"
        );
    }
}

#[cfg(test)]
mod task_manager_tests {
    use super::*;

    #[tokio::test]
    async fn old_task_cleanup_keeps_replacement_registered() {
        let manager = UserTaskManager::new();
        let (release_tx, release_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let old_manager = manager.clone();
        let old = tokio::spawn(async move {
            let _ = release_rx.await;
            old_manager.cleanup_generation("slack:U1", 1).await;
            let _ = done_tx.send(());
        });
        manager.tasks.lock().await.insert(
            "slack:U1".into(),
            ActiveTask {
                handle: old,
                generation: 1,
            },
        );
        let replacement = tokio::spawn(std::future::pending());
        manager.tasks.lock().await.insert(
            "slack:U1".into(),
            ActiveTask {
                handle: replacement,
                generation: 2,
            },
        );

        release_tx.send(()).unwrap();
        done_rx.await.unwrap();

        assert_eq!(
            manager
                .tasks
                .lock()
                .await
                .get("slack:U1")
                .unwrap()
                .generation,
            2
        );
        manager
            .tasks
            .lock()
            .await
            .remove("slack:U1")
            .unwrap()
            .handle
            .abort();
    }
}

#[cfg(test)]
mod runtime_store_tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    use crate::config::{Config, Paths};
    use crate::cron::{CronConfig, CronService, SystemClock};
    use crate::pairing::PairingStore;
    use crate::sandbox::{SandboxProvider, TurnResult};

    struct BlockingProvider {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    struct SerialProvider {
        entered: tokio::sync::mpsc::UnboundedSender<String>,
        releases: Mutex<VecDeque<oneshot::Receiver<()>>>,
        calls: AtomicU64,
    }

    #[async_trait]
    impl SandboxProvider for SerialProvider {
        async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
            self.entered.send(job.prompt).unwrap();
            let release = self.releases.lock().unwrap().pop_front().unwrap();
            release.await.unwrap();
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TurnResult {
                response: "done".into(),
                backend_session_id: format!("sess-{call}"),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl SandboxProvider for BlockingProvider {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let release = self.release.lock().unwrap().take().unwrap();
            let _ = release.await;
            Ok(TurnResult {
                response: "done".into(),
                backend_session_id: "sess-1".into(),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    fn runtime(paths: &Paths) -> (Arc<Runtime>, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let config = Arc::new(Config::default());
        let paths = Arc::new(paths.clone());
        let pairing = PairingStore::load(&paths).unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let provider: Arc<dyn SandboxProvider> = Arc::new(BlockingProvider {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        let cron = CronService::new(
            SystemClock,
            CronConfig::default(),
            config.clone(),
            paths.clone(),
            provider.clone(),
        )
        .unwrap();
        (
            Arc::new(Runtime {
                config,
                paths,
                provider,
                pairing: std::sync::Mutex::new(pairing),
                session_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
                session_ticket: std::sync::atomic::AtomicU64::new(0),
                cron,
            }),
            entered_rx,
            release_tx,
        )
    }

    #[tokio::test]
    async fn approval_during_turn_survives_turn_completion() {
        let (_temp, paths) = crate::config::test_paths();
        let mut seed = PairingStore::load(&paths).unwrap();
        let code = seed
            .modify(|store| store.get_or_create_pending("telegram", "2", None, None))
            .unwrap()
            .0;
        let (rt, entered, release) = runtime(&paths);
        let turn_rt = rt.clone();
        let turn = tokio::spawn(async move {
            query_ai_with_session(
                &turn_rt,
                "telegram",
                "1",
                Affinity::Chat {
                    channel: "telegram".into(),
                    user: "1".into(),
                },
                "hello",
                String::new(),
                None,
                Vec::new(),
            )
            .await
        });
        entered.await.unwrap();
        PairingStore::load(&paths)
            .unwrap()
            .modify(|store| store.approve(&code))
            .unwrap();
        release.send(()).unwrap();
        turn.await.unwrap().unwrap();

        let stored = PairingStore::load(&paths).unwrap();
        assert!(stored.is_approved("telegram", "2"));
        assert_eq!(stored.sessions.get("telegram:1").unwrap(), "sess-1");
    }

    #[tokio::test]
    async fn session_write_is_compare_and_set() {
        let (_temp, paths) = crate::config::test_paths();
        PairingStore::load(&paths)
            .unwrap()
            .modify(|store| {
                store.sessions.insert("telegram:1".into(), "old".into());
                Ok(())
            })
            .unwrap();
        let (rt, entered, release) = runtime(&paths);
        let turn_rt = rt.clone();
        let turn = tokio::spawn(async move {
            query_ai_with_session(
                &turn_rt,
                "telegram",
                "1",
                Affinity::Chat {
                    channel: "telegram".into(),
                    user: "1".into(),
                },
                "hello",
                String::new(),
                None,
                Vec::new(),
            )
            .await
        });
        entered.await.unwrap();
        process_command(&rt, "telegram", "1", "/new", true, None).unwrap();
        release.send(()).unwrap();
        turn.await.unwrap().unwrap();
        assert!(
            !PairingStore::load(&paths)
                .unwrap()
                .sessions
                .contains_key("telegram:1")
        );
    }

    #[tokio::test]
    async fn slack_thread_new_clears_thread_session_only() {
        let (_temp, paths) = crate::config::test_paths();
        let (rt, _entered, _release) = runtime(&paths);
        lock(&rt.pairing)
            .modify(|store| {
                store
                    .sessions
                    .insert("slack:thread:C1:1.2".into(), "thread".into());
                store.sessions.insert("slack:U1".into(), "direct".into());
                Ok(())
            })
            .unwrap();

        process_command(
            &rt,
            "slack",
            "U1",
            "/new",
            true,
            Some("slack:thread:C1:1.2"),
        )
        .unwrap();

        let stored = PairingStore::load(&paths).unwrap();
        assert!(!stored.sessions.contains_key("slack:thread:C1:1.2"));
        assert_eq!(
            stored.sessions.get("slack:U1").map(String::as_str),
            Some("direct")
        );
    }

    #[tokio::test]
    async fn session_write_succeeds_when_unchanged() {
        let (_temp, paths) = crate::config::test_paths();
        let (rt, entered, release) = runtime(&paths);
        let turn = tokio::spawn(async move {
            query_ai_with_session(
                &rt,
                "telegram",
                "1",
                Affinity::Chat {
                    channel: "telegram".into(),
                    user: "1".into(),
                },
                "hello",
                String::new(),
                None,
                Vec::new(),
            )
            .await
        });
        entered.await.unwrap();
        release.send(()).unwrap();
        turn.await.unwrap().unwrap();
        assert_eq!(
            PairingStore::load(&paths)
                .unwrap()
                .sessions
                .get("telegram:1")
                .unwrap(),
            "sess-1"
        );
    }

    #[tokio::test]
    async fn same_slack_thread_calls_serialize_through_cas() {
        let (_temp, paths) = crate::config::test_paths();
        let config = Arc::new(Config::default());
        let paths = Arc::new(paths);
        let pairing = PairingStore::load(&paths).unwrap();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release1_tx, release1_rx) = oneshot::channel();
        let (release2_tx, release2_rx) = oneshot::channel();
        let provider: Arc<dyn SandboxProvider> = Arc::new(SerialProvider {
            entered: entered_tx,
            releases: Mutex::new(VecDeque::from([release1_rx, release2_rx])),
            calls: AtomicU64::new(0),
        });
        let cron = CronService::new(
            SystemClock,
            CronConfig::default(),
            config.clone(),
            paths.clone(),
            provider.clone(),
        )
        .unwrap();
        let rt = Arc::new(Runtime {
            config,
            paths,
            provider,
            pairing: std::sync::Mutex::new(pairing),
            session_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
            session_ticket: AtomicU64::new(0),
            cron,
        });
        let key = "slack:thread:C1:1.2";
        let first_rt = rt.clone();
        let first = tokio::spawn(async move {
            query_ai_with_session(
                &first_rt,
                "slack",
                "U1",
                Affinity::SlackThread {
                    channel_id: "C1".into(),
                    thread_ts: "1.2".into(),
                },
                "first",
                String::new(),
                Some(key),
                Vec::new(),
            )
            .await
        });
        assert_eq!(entered_rx.recv().await.as_deref(), Some("first"));
        let second_rt = rt.clone();
        let second = tokio::spawn(async move {
            query_ai_with_session(
                &second_rt,
                "slack",
                "U2",
                Affinity::SlackThread {
                    channel_id: "C1".into(),
                    thread_ts: "1.2".into(),
                },
                "second",
                String::new(),
                Some(key),
                Vec::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(entered_rx.try_recv().is_err());
        release1_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        assert_eq!(entered_rx.recv().await.as_deref(), Some("second"));
        assert_eq!(
            PairingStore::load(&rt.paths)
                .unwrap()
                .sessions
                .get(key)
                .map(String::as_str),
            Some("sess-1")
        );
        release2_tx.send(()).unwrap();
        second.await.unwrap().unwrap();
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "attachment path does not resolve to a file: missing.png")]
    fn prompt_path_must_resolve_to_a_file() {
        let base = tempfile::tempdir().unwrap();
        let prompt = (
            "look\n\n@missing.png".to_string(),
            vec!["missing.png".to_string()],
        );

        assert_prompt_paths_resolve(base.path(), &prompt);
    }

    #[test]
    fn images_are_referenced_by_workspace_relative_path() {
        let base = PathBuf::from("/home/ubuntu/.config/cica");
        let (text, names) = build_text_with_images(
            &base,
            "what is wrong here?",
            &[PathBuf::from(
                "/home/ubuntu/.config/cica/internal/slack_attachments/F1_shot.png",
            )],
        );

        assert!(!text.contains("/home/ubuntu"), "got: {text}");
        assert!(
            text.contains("@internal/slack_attachments/F1_shot.png"),
            "got: {text}"
        );
        assert_eq!(
            names,
            vec!["internal/slack_attachments/F1_shot.png".to_string()]
        );
    }

    #[test]
    fn names_match_every_referenced_file() {
        let base = PathBuf::from("/router");
        let (text, names) = build_text_with_images(
            &base,
            "two",
            &[
                PathBuf::from("/router/internal/slack_attachments/A one.png"),
                PathBuf::from("/router/internal/slack_attachments/B.png"),
            ],
        );
        assert_eq!(
            names,
            vec![
                "internal/slack_attachments/A one.png".to_string(),
                "internal/slack_attachments/B.png".to_string()
            ]
        );
        assert!(text.contains("@internal/slack_attachments/A one.png"));
        assert!(text.contains("@internal/slack_attachments/B.png"));
    }

    #[test]
    fn telegram_image_uses_its_workspace_relative_path() {
        let base = PathBuf::from("/workspace");
        let (text, attachments) = build_text_with_images(
            &base,
            "photo",
            &[base.join("internal/telegram_attachments/x.jpg")],
        );

        assert!(text.contains("@internal/telegram_attachments/x.jpg"));
        assert_eq!(
            attachments,
            vec!["internal/telegram_attachments/x.jpg".to_string()]
        );
    }

    #[test]
    fn image_outside_workspace_stays_absolute_and_is_not_shipped() {
        let base = PathBuf::from("/workspace");
        let outside = PathBuf::from("/tmp/x.jpg");
        let (text, attachments) = build_text_with_images(&base, "photo", &[outside]);

        assert!(text.contains("@/tmp/x.jpg"));
        assert!(attachments.is_empty());
    }
}
