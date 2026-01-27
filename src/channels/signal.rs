//! Signal channel implementation using signal-cli daemon

use anyhow::{Context, Result, anyhow, bail};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::{
    CommandResult, UserTaskManager, handle_onboarding, process_command, query_claude_with_session,
    reindex_user_memories,
};
use crate::config::{self, SignalConfig};
use crate::onboarding;
use crate::pairing::PairingStore;
use crate::setup;

const DAEMON_PORT: u16 = 18080;
const PID_FILE_NAME: &str = "cica-signal-daemon.pid";

/// signal-cli daemon manager
struct SignalDaemon {
    process: Child,
    pid_file: PathBuf,
}

impl SignalDaemon {
    /// Get the PID file path
    fn pid_file_path() -> Result<PathBuf> {
        Ok(config::paths()?.signal_data_dir.join(PID_FILE_NAME))
    }

    /// Check if an existing daemon is running (and still alive)
    fn check_existing() -> Option<u32> {
        let pid_file = Self::pid_file_path().ok()?;
        if !pid_file.exists() {
            return None;
        }

        let pid_str = std::fs::read_to_string(&pid_file).ok()?;
        let pid: u32 = pid_str.trim().parse().ok()?;

        // Check if process is still running
        #[cfg(unix)]
        {
            use std::process::Command as StdCommand;
            let status = StdCommand::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .ok()?;
            if status.success() {
                return Some(pid);
            }
        }

        // PID file exists but process is dead - clean up
        let _ = std::fs::remove_file(&pid_file);
        None
    }

    /// Check if daemon HTTP endpoint is responding
    async fn is_daemon_ready() -> bool {
        let url = format!("http://127.0.0.1:{}/api/v1/rpc", DAEMON_PORT);
        reqwest::get(&url).await.is_ok()
    }

    /// Start signal-cli daemon with JSON-RPC HTTP interface
    async fn start(phone_number: &str) -> Result<Self> {
        let paths = config::paths()?;
        let pid_file = Self::pid_file_path()?;

        // Check if daemon is already running
        if let Some(pid) = Self::check_existing() {
            // Verify it's actually responding
            if Self::is_daemon_ready().await {
                bail!(
                    "signal-cli daemon is already running (PID {}). \
                     Kill it first or let cica manage it.",
                    pid
                );
            } else {
                // PID exists but not responding - kill and restart
                warn!("Found stale daemon PID {}, cleaning up...", pid);
                #[cfg(unix)]
                {
                    use std::process::Command as StdCommand;
                    let _ = StdCommand::new("kill").arg(pid.to_string()).status();
                }
                let _ = std::fs::remove_file(&pid_file);
            }
        }

        let java = setup::find_java().ok_or_else(|| anyhow!("Java not found. Run setup first."))?;
        let signal_cli = setup::find_signal_cli()
            .ok_or_else(|| anyhow!("signal-cli not found. Run setup first."))?;

        // Get the signal-cli lib directory (parent of bin)
        let signal_cli_home = signal_cli
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("Could not determine signal-cli home directory"))?;

        info!("Starting signal-cli daemon on port {}...", DAEMON_PORT);

        // Build JAVA_HOME from java binary path
        let java_home = java
            .parent() // bin
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("Could not determine JAVA_HOME"))?;

        // Ensure data directory exists
        std::fs::create_dir_all(&paths.signal_data_dir)?;

        // Start signal-cli daemon
        // Use --receive-mode manual so we can poll with the receive RPC method
        let http_addr = format!("localhost:{}", DAEMON_PORT);
        let process = Command::new(&signal_cli)
            .args([
                "-a",
                phone_number,
                "--config",
                paths.signal_data_dir.to_str().unwrap(),
                "daemon",
                "--http",
                &http_addr,
                "--receive-mode",
                "manual",
            ])
            .env("JAVA_HOME", java_home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    java.parent().unwrap().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("SIGNAL_CLI_HOME", signal_cli_home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start signal-cli daemon")?;

        // Write PID file
        if let Some(pid) = process.id() {
            std::fs::write(&pid_file, pid.to_string())?;
        }

        let mut daemon = Self { process, pid_file };

        // Wait for daemon to be ready
        daemon.wait_for_ready().await?;

        Ok(daemon)
    }

    /// Wait for the daemon HTTP server to become available
    async fn wait_for_ready(&mut self) -> Result<()> {
        for i in 0..30 {
            sleep(Duration::from_millis(500)).await;

            // Check if process has exited
            if let Ok(Some(status)) = self.process.try_wait() {
                // Process exited, try to get stderr
                let stderr = self.process.stderr.take();
                let stderr_msg = if let Some(mut stderr) = stderr {
                    use tokio::io::AsyncReadExt;
                    let mut buf = String::new();
                    let _ = stderr.read_to_string(&mut buf).await;
                    buf
                } else {
                    String::new()
                };
                bail!(
                    "signal-cli daemon exited with status {}: {}",
                    status,
                    stderr_msg.trim()
                );
            }

            if Self::is_daemon_ready().await {
                info!("signal-cli daemon is ready");
                return Ok(());
            }
            debug!("Waiting for signal-cli daemon... attempt {}", i + 1);
        }

        bail!("signal-cli daemon failed to start within 15 seconds")
    }

    /// Get the JSON-RPC endpoint URL
    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1/rpc", DAEMON_PORT)
    }

    /// Gracefully shutdown the daemon
    async fn shutdown(&mut self) {
        info!("Shutting down signal-cli daemon...");

        // Try graceful termination first
        #[cfg(unix)]
        if let Some(pid) = self.process.id() {
            use std::process::Command as StdCommand;
            let _ = StdCommand::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();

            // Wait a bit for graceful shutdown
            for _ in 0..10 {
                sleep(Duration::from_millis(200)).await;
                if self.process.try_wait().ok().flatten().is_some() {
                    break;
                }
            }
        }

        // Force kill if still running
        let _ = self.process.kill().await;

        // Clean up PID file
        let _ = std::fs::remove_file(&self.pid_file);

        info!("signal-cli daemon stopped");
    }
}

impl Drop for SignalDaemon {
    fn drop(&mut self) {
        // Synchronous cleanup - try to kill the process
        let _ = self.process.start_kill();
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// Message received from Signal
#[derive(Debug, Deserialize)]
struct SignalMessage {
    envelope: Option<Envelope>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    source: Option<String>,
    #[serde(rename = "sourceNumber")]
    source_number: Option<String>,
    #[serde(rename = "sourceUuid")]
    source_uuid: Option<String>,
    #[serde(rename = "sourceName")]
    source_name: Option<String>,
    #[serde(rename = "dataMessage")]
    data_message: Option<DataMessage>,
}

#[derive(Debug, Deserialize)]
struct DataMessage {
    message: Option<String>,
    attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Deserialize)]
struct Attachment {
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    id: Option<String>,
    #[serde(rename = "filename")]
    filename: Option<String>,
    size: Option<u64>,
}

/// Run the Signal bot
pub async fn run(config: SignalConfig) -> Result<()> {
    info!("Starting Signal bot for {}...", config.phone_number);

    // Start the signal-cli daemon
    let mut daemon = SignalDaemon::start(&config.phone_number).await?;

    // Create JSON-RPC client
    let client = Arc::new(
        HttpClientBuilder::default()
            .build(daemon.rpc_url())
            .context("Failed to create JSON-RPC client")?,
    );

    info!("Signal bot running. Listening for messages...");

    // Create shared task manager for per-user message handling
    let task_manager = UserTaskManager::new();

    // Set up graceful shutdown
    let result = run_message_loop(client, &config.phone_number, task_manager).await;

    // Shutdown daemon gracefully
    daemon.shutdown().await;

    result
}

/// Main message polling loop
async fn run_message_loop(
    client: Arc<HttpClient>,
    phone_number: &str,
    task_manager: Arc<UserTaskManager>,
) -> Result<()> {
    loop {
        match receive_messages(&client, phone_number).await {
            Ok(messages) => {
                for msg in messages {
                    if let Err(e) =
                        handle_message(client.clone(), phone_number, msg, Arc::clone(&task_manager))
                            .await
                    {
                        error!("Error handling message: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("Error receiving messages: {}", e);
            }
        }

        // Poll interval
        sleep(Duration::from_secs(1)).await;
    }
}

/// Receive pending messages
async fn receive_messages(client: &HttpClient, _account: &str) -> Result<Vec<SignalMessage>> {
    // In single-account daemon mode, we don't pass account parameter
    let mut params = ObjectParams::new();
    params.insert("timeout", 1)?; // 1 second timeout

    let result: Value = client
        .request("receive", params)
        .await
        .context("Failed to receive messages")?;

    // Parse the response - it's an array of message envelopes
    let messages: Vec<SignalMessage> = serde_json::from_value(result).unwrap_or_default();

    Ok(messages)
}

/// Send a message to a recipient
async fn send_message(
    client: &HttpClient,
    _account: &str,
    recipient: &str,
    message: &str,
) -> Result<()> {
    // In single-account daemon mode, we don't pass account parameter
    let mut params = ObjectParams::new();
    params.insert("recipient", vec![recipient])?;
    params.insert("message", message)?;

    let _: Value = client
        .request("send", params)
        .await
        .context("Failed to send message")?;

    Ok(())
}

/// Send a typing indicator to a recipient
async fn send_typing(client: &HttpClient, recipient: &str) -> Result<()> {
    let mut params = ObjectParams::new();
    params.insert("recipient", vec![recipient])?;

    let _: Value = client
        .request("sendTyping", params)
        .await
        .context("Failed to send typing indicator")?;

    Ok(())
}

/// Start sending periodic typing indicators until cancelled.
/// Returns a sender that, when dropped or sent to, stops the typing loop.
fn start_typing_indicator(client: Arc<HttpClient>, recipient: String) -> oneshot::Sender<()> {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            // Send typing indicator (lasts 15 seconds on Signal)
            let _ = send_typing(&client, &recipient).await;

            // Wait 10 seconds or until cancelled
            tokio::select! {
                _ = sleep(Duration::from_secs(10)) => {
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

/// Get the path where signal-cli stores attachments
fn get_attachment_path(attachment_id: &str) -> Option<PathBuf> {
    let paths = config::paths().ok()?;
    let attachment_path = paths
        .signal_data_dir
        .join("attachments")
        .join(attachment_id);
    if attachment_path.exists() {
        Some(attachment_path)
    } else {
        None
    }
}

/// Check if a content type is an image type that Claude can process
fn is_image_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

/// Handle an incoming message
async fn handle_message(
    client: Arc<HttpClient>,
    account: &str,
    msg: SignalMessage,
    task_manager: Arc<UserTaskManager>,
) -> Result<()> {
    let envelope = match msg.envelope {
        Some(e) => e,
        None => return Ok(()),
    };

    // Get sender info - prefer phone number, fall back to UUID
    let sender = envelope
        .source_number
        .or(envelope.source_uuid)
        .or(envelope.source)
        .unwrap_or_default();

    if sender.is_empty() {
        return Ok(());
    }

    // Extract message content and attachments
    let data_message = match envelope.data_message {
        Some(dm) => dm,
        None => return Ok(()),
    };

    let text = data_message.message.clone().unwrap_or_default();
    let attachments = data_message.attachments.unwrap_or_default();

    // Collect image attachment paths
    let image_paths: Vec<PathBuf> = attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_ref()
                .map(|ct| is_image_content_type(ct))
                .unwrap_or(false)
        })
        .filter_map(|a| a.id.as_ref().and_then(|id| get_attachment_path(id)))
        .collect();

    // Skip if no text and no images
    if text.is_empty() && image_paths.is_empty() {
        return Ok(());
    }

    let display_name = envelope.source_name;

    info!("Message from {}: {}", sender, text);

    // Check if user is approved
    let mut store = PairingStore::load()?;

    if !store.is_approved("signal", &sender) {
        // Create or get existing pairing request
        let (code, _) = store.get_or_create_pending("signal", &sender, None, display_name)?;

        let response = format!(
            "Hi! I don't recognize you yet.\n\n\
            Pairing code: {}\n\n\
            Ask the owner to run:\n\
            cica approve {}",
            code, code
        );

        send_message(&client, account, &sender, &response).await?;
        return Ok(());
    }

    // Check if onboarding is complete for this user
    let onboarding_complete = onboarding::is_complete_for_user("signal", &sender)?;

    // Check for commands first (works even during onboarding)
    if let CommandResult::Response(response) =
        process_command(&mut store, "signal", &sender, &text, onboarding_complete)?
    {
        send_message(&client, account, &sender, &response).await?;
        return Ok(());
    }

    if !onboarding_complete {
        let response = handle_onboarding("signal", &sender, &text).await?;
        send_message(&client, account, &sender, &response).await?;
        return Ok(());
    }

    // Queue the message for processing with debounce and interruption support
    let user_key = format!("signal:{}", sender);
    let client_clone = client.clone();
    let account_owned = account.to_string();
    let sender_clone = sender.clone();

    // Build the message text with image references
    // Images are referenced using @path syntax which Claude Code understands
    let mut text_with_images = text.clone();
    for (i, path) in image_paths.iter().enumerate() {
        if let Some(path_str) = path.to_str() {
            if text_with_images.is_empty() {
                text_with_images = format!("@{}", path_str);
            } else if i == 0 {
                text_with_images = format!("{}\n\n@{}", text_with_images, path_str);
            } else {
                text_with_images = format!("{} @{}", text_with_images, path_str);
            }
        }
    }

    // Log that we're processing images
    if !image_paths.is_empty() {
        info!(
            "Message includes {} image(s): {:?}",
            image_paths.len(),
            image_paths
        );
    }

    task_manager
        .process_message(user_key, text_with_images, move |messages| async move {
            // Combine multiple messages if batched
            let combined_text = messages.join("\n\n");

            // Start periodic typing indicator
            let typing_cancel = start_typing_indicator(client_clone.clone(), sender_clone.clone());

            // Query Claude with context
            let context_prompt = match onboarding::build_context_prompt_for_user(
                Some("Signal"),
                Some("signal"),
                Some(&sender_clone),
                Some(&combined_text),
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to build context prompt: {}", e);
                    drop(typing_cancel);
                    let _ = send_message(
                        &client_clone,
                        &account_owned,
                        &sender_clone,
                        &format!("Sorry, I encountered an error: {}", e),
                    )
                    .await;
                    return;
                }
            };

            let mut store = match PairingStore::load() {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to load pairing store: {}", e);
                    drop(typing_cancel);
                    let _ = send_message(
                        &client_clone,
                        &account_owned,
                        &sender_clone,
                        &format!("Sorry, I encountered an error: {}", e),
                    )
                    .await;
                    return;
                }
            };

            let (response, _session_id) = match query_claude_with_session(
                &mut store,
                "signal",
                &sender_clone,
                &combined_text,
                context_prompt,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("Claude query failed: {}", e);
                    drop(typing_cancel);
                    let _ = send_message(
                        &client_clone,
                        &account_owned,
                        &sender_clone,
                        &format!("Sorry, I encountered an error: {}", e),
                    )
                    .await;
                    return;
                }
            };

            // Stop typing indicator before sending response
            drop(typing_cancel);

            if let Err(e) =
                send_message(&client_clone, &account_owned, &sender_clone, &response).await
            {
                warn!("Failed to send message: {}", e);
            }

            // Re-index memories in case Claude saved new ones
            reindex_user_memories("signal", &sender_clone);
        })
        .await;

    Ok(())
}

/// Result of registration attempt
pub enum RegistrationResult {
    /// Registration succeeded, SMS sent
    Success,
    /// CAPTCHA required - user needs to solve it
    CaptchaRequired,
    /// Already registered
    AlreadyRegistered,
    /// Authorization failed - number may be registered elsewhere
    AuthorizationFailed,
    /// Rate limited - too many attempts
    RateLimited,
}

/// Register a new Signal account (called during setup)
pub async fn register_account(
    phone_number: &str,
    captcha: Option<&str>,
    use_voice: bool,
) -> Result<RegistrationResult> {
    let paths = config::paths()?;
    let java = setup::find_java().ok_or_else(|| anyhow!("Java not found"))?;
    let signal_cli = setup::find_signal_cli().ok_or_else(|| anyhow!("signal-cli not found"))?;

    // Ensure data directory exists
    std::fs::create_dir_all(&paths.signal_data_dir)?;

    let java_home = java
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("Could not determine JAVA_HOME"))?;

    info!("Registering Signal account for {}...", phone_number);

    let mut args = vec![
        "-a",
        phone_number,
        "--config",
        paths.signal_data_dir.to_str().unwrap(),
        "register",
    ];

    // Add voice flag if requested (voice call instead of SMS)
    if use_voice {
        args.push("-v");
    }

    // Add captcha if provided
    let captcha_owned: String;
    if let Some(c) = captcha {
        captcha_owned = c.to_string();
        args.push("--captcha");
        args.push(&captcha_owned);
        debug!(
            "Using captcha token (first 50 chars): {}...",
            &captcha_owned[..captcha_owned.len().min(50)]
        );
    }

    let output = Command::new(&signal_cli)
        .args(&args)
        .env("JAVA_HOME", java_home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                java.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .await
        .context("Failed to run signal-cli register")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);
    let combined_lower = combined.to_lowercase();

    // Log for debugging
    debug!("Registration stdout: {}", stdout);
    debug!("Registration stderr: {}", stderr);
    debug!("Registration exit status: {}", output.status);

    if output.status.success() {
        return Ok(RegistrationResult::Success);
    }

    // Check for captcha requirement - but only if we didn't already provide one
    // If we provided a captcha and still get this error, the captcha was invalid
    if combined_lower.contains("captcha") {
        if captcha.is_some() {
            // We already provided a captcha but it failed - report specific error
            bail!(
                "CAPTCHA verification failed. The token may have expired or been invalid.\n\
                 Please try again with a fresh CAPTCHA.\n\
                 signal-cli output: {}",
                combined.trim()
            );
        }
        return Ok(RegistrationResult::CaptchaRequired);
    }

    if combined_lower.contains("already registered") {
        return Ok(RegistrationResult::AlreadyRegistered);
    }

    // Authorization failed usually means the number is registered on another device
    if combined_lower.contains("authorization failed") || combined_lower.contains("403") {
        return Ok(RegistrationResult::AuthorizationFailed);
    }

    // Rate limited
    if combined_lower.contains("rate limit") || combined_lower.contains("429") {
        return Ok(RegistrationResult::RateLimited);
    }

    bail!("Registration failed: {}", combined.trim());
}

/// Verify a Signal account with SMS code (called during setup)
pub async fn verify_account(phone_number: &str, code: &str) -> Result<()> {
    let paths = config::paths()?;
    let java = setup::find_java().ok_or_else(|| anyhow!("Java not found"))?;
    let signal_cli = setup::find_signal_cli().ok_or_else(|| anyhow!("signal-cli not found"))?;

    let java_home = java
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("Could not determine JAVA_HOME"))?;

    info!("Verifying Signal account...");

    let output = Command::new(&signal_cli)
        .args([
            "-a",
            phone_number,
            "--config",
            paths.signal_data_dir.to_str().unwrap(),
            "verify",
            code,
        ])
        .env("JAVA_HOME", java_home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                java.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .await
        .context("Failed to run signal-cli verify")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Verification failed: {}", stderr);
    }

    Ok(())
}
