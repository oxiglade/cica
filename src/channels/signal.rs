//! Signal channel implementation using signal-cli daemon

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
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
    Channel, TypingGuard, UserTaskManager, build_text_with_images, determine_action,
    execute_action, execute_claude_query,
};
use crate::config::{self, SignalConfig};
use crate::pairing::PairingStore;
use crate::setup;

// ============================================================================
// Channel Implementation
// ============================================================================

/// Signal channel implementation
pub struct SignalChannel {
    client: Arc<HttpClient>,
    recipient: String,
}

impl SignalChannel {
    pub fn new(client: Arc<HttpClient>, recipient: String) -> Self {
        Self { client, recipient }
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &'static str {
        "signal"
    }

    fn display_name(&self) -> &'static str {
        "Signal"
    }

    async fn send_message(&self, message: &str) -> Result<()> {
        self.send_message_with_attachments(message, &[]).await
    }

    async fn send_message_with_attachments(
        &self,
        message: &str,
        attachment_paths: &[PathBuf],
    ) -> Result<()> {
        let mut params = ObjectParams::new();
        params.insert("recipient", vec![self.recipient.as_str()])?;
        params.insert("message", message)?;

        // Add attachments if any
        if !attachment_paths.is_empty() {
            let attachment_strings: Vec<String> = attachment_paths
                .iter()
                .filter_map(|p| p.to_str().map(|s| s.to_string()))
                .collect();
            params.insert("attachments", attachment_strings)?;
        }

        let _: Value = self
            .client
            .request("send", params)
            .await
            .context("Failed to send message")?;

        Ok(())
    }

    fn start_typing(&self) -> TypingGuard {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let client = self.client.clone();
        let recipient = self.recipient.clone();

        tokio::spawn(async move {
            loop {
                // Send typing indicator (lasts 15 seconds on Signal)
                let mut params = ObjectParams::new();
                if params.insert("recipient", vec![recipient.as_str()]).is_ok() {
                    let _: Result<Value, _> = client.request("sendTyping", params).await;
                }

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

        TypingGuard::new(cancel_tx)
    }
}

// ============================================================================
// Daemon Management
// ============================================================================

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

// ============================================================================
// Message Types
// ============================================================================

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
#[allow(dead_code)]
struct Attachment {
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    id: Option<String>,
    #[serde(rename = "filename")]
    filename: Option<String>,
    size: Option<u64>,
}

// ============================================================================
// Public API
// ============================================================================

/// Run the Signal bot
pub async fn run(config: SignalConfig) -> Result<()> {
    info!("Starting Signal bot for {}...", config.phone_number);

    // Create shared task manager for per-user message handling (persists across restarts)
    let task_manager = UserTaskManager::new();

    // Outer loop for daemon recovery
    loop {
        // Start the signal-cli daemon
        let mut daemon = match SignalDaemon::start(&config.phone_number).await {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to start signal-cli daemon: {:#}", e);
                info!("Retrying in 10 seconds...");
                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // Create JSON-RPC client with longer timeouts to handle contention
        let client = Arc::new(
            HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(daemon.rpc_url())
                .context("Failed to create JSON-RPC client")?,
        );

        info!("Signal bot running. Listening for messages...");

        // Run message loop until it signals a restart is needed
        let needs_restart = run_message_loop(client, Arc::clone(&task_manager)).await;

        // Shutdown daemon gracefully
        daemon.shutdown().await;

        if needs_restart {
            warn!("Restarting signal-cli daemon due to repeated failures...");
            sleep(Duration::from_secs(2)).await;
        } else {
            // Clean exit requested
            break;
        }
    }

    Ok(())
}

// ============================================================================
// Message Handling
// ============================================================================

/// Maximum consecutive receive failures before restarting daemon
const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Main message polling loop
/// Returns true if daemon should be restarted, false for clean exit
async fn run_message_loop(client: Arc<HttpClient>, task_manager: Arc<UserTaskManager>) -> bool {
    let mut consecutive_failures: u32 = 0;

    loop {
        match receive_messages(&client).await {
            Ok(messages) => {
                // Reset failure counter on success
                consecutive_failures = 0;

                for msg in messages {
                    if let Err(e) =
                        handle_message(client.clone(), msg, Arc::clone(&task_manager)).await
                    {
                        error!("Error handling message: {}", e);
                    }
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    "Error receiving messages ({}/{}): {:#}",
                    consecutive_failures, MAX_CONSECUTIVE_FAILURES, e
                );

                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    error!(
                        "Too many consecutive receive failures ({}), triggering daemon restart",
                        consecutive_failures
                    );
                    return true; // Signal restart needed
                }
            }
        }

        // Poll interval
        sleep(Duration::from_secs(1)).await;
    }
}

/// Receive pending messages
async fn receive_messages(client: &HttpClient) -> Result<Vec<SignalMessage>> {
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
    if !image_paths.is_empty() {
        info!(
            "Message includes {} image(s): {:?}",
            image_paths.len(),
            image_paths
        );
    }

    // Create channel wrapper
    let channel: Arc<dyn Channel> = Arc::new(SignalChannel::new(client, sender.clone()));

    // Determine what action to take
    let mut store = PairingStore::load()?;
    let action = determine_action(
        channel.name(),
        &sender,
        &text,
        &image_paths,
        &mut store,
        None, // Signal doesn't have usernames
        display_name,
    )?;

    // Execute the action
    if let Some(query_text) = execute_action(channel.as_ref(), &sender, action).await? {
        // QueryClaude action - queue with task manager for debouncing
        let text_with_images = build_text_with_images(&query_text, &image_paths);
        let user_key = format!("{}:{}", channel.name(), sender);
        let channel_clone = channel.clone();
        let sender_clone = sender.clone();

        task_manager
            .process_message(user_key, text_with_images, move |messages| async move {
                execute_claude_query(channel_clone, &sender_clone, messages).await;
            })
            .await;
    }

    Ok(())
}

// ============================================================================
// Registration
// ============================================================================

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
