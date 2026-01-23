//! Signal channel implementation using signal-cli daemon

use anyhow::{Context, Result, anyhow, bail};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::claude;
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
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("Could not determine JAVA_HOME"))?;

        // Ensure data directory exists
        std::fs::create_dir_all(&paths.signal_data_dir)?;

        // Start signal-cli daemon
        let process = Command::new(&signal_cli)
            .args([
                "-a",
                phone_number,
                "--config",
                paths.signal_data_dir.to_str().unwrap(),
                "daemon",
                "--http",
                "--http-port",
                &DAEMON_PORT.to_string(),
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

        let daemon = Self { process, pid_file };

        // Wait for daemon to be ready
        daemon.wait_for_ready().await?;

        Ok(daemon)
    }

    /// Wait for the daemon HTTP server to become available
    async fn wait_for_ready(&self) -> Result<()> {
        for i in 0..30 {
            sleep(Duration::from_millis(500)).await;

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
    #[serde(rename = "sourceName")]
    source_name: Option<String>,
    #[serde(rename = "dataMessage")]
    data_message: Option<DataMessage>,
}

#[derive(Debug, Deserialize)]
struct DataMessage {
    message: Option<String>,
}

/// Run the Signal bot
pub async fn run(config: SignalConfig) -> Result<()> {
    info!("Starting Signal bot for {}...", config.phone_number);

    // Start the signal-cli daemon
    let mut daemon = SignalDaemon::start(&config.phone_number).await?;

    // Create JSON-RPC client
    let client = HttpClientBuilder::default()
        .build(&daemon.rpc_url())
        .context("Failed to create JSON-RPC client")?;

    info!("Signal bot running. Listening for messages...");

    // Set up graceful shutdown
    let result = run_message_loop(&client, &config.phone_number).await;

    // Shutdown daemon gracefully
    daemon.shutdown().await;

    result
}

/// Main message polling loop
async fn run_message_loop(client: &HttpClient, phone_number: &str) -> Result<()> {
    loop {
        match receive_messages(client, phone_number).await {
            Ok(messages) => {
                for msg in messages {
                    if let Err(e) = handle_message(client, phone_number, msg).await {
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
async fn receive_messages(client: &HttpClient, account: &str) -> Result<Vec<SignalMessage>> {
    let mut params = ObjectParams::new();
    params.insert("account", account)?;

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
    account: &str,
    recipient: &str,
    message: &str,
) -> Result<()> {
    let mut params = ObjectParams::new();
    params.insert("account", account)?;
    params.insert("recipients", vec![recipient])?;
    params.insert("message", message)?;

    let _: Value = client
        .request("send", params)
        .await
        .context("Failed to send message")?;

    Ok(())
}

/// Handle an incoming message
async fn handle_message(client: &HttpClient, account: &str, msg: SignalMessage) -> Result<()> {
    let envelope = match msg.envelope {
        Some(e) => e,
        None => return Ok(()),
    };

    // Get sender info
    let sender = envelope
        .source_number
        .or(envelope.source)
        .unwrap_or_default();

    if sender.is_empty() {
        return Ok(());
    }

    // Get message text
    let text = match envelope.data_message.and_then(|d| d.message) {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(()), // No text message
    };

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

        send_message(client, account, &sender, &response).await?;
        return Ok(());
    }

    // Check if onboarding is complete
    if !onboarding::is_complete()? {
        let response = handle_onboarding(&text).await?;
        send_message(client, account, &sender, &response).await?;
        return Ok(());
    }

    // Check if we have an existing session to resume
    let existing_session = store.sessions.get(&format!("signal:{}", sender)).cloned();

    // Query Claude with context (and resume if we have a session)
    let context_prompt = onboarding::build_context_prompt(Some("Signal"))?;
    let options = claude::QueryOptions {
        system_prompt: Some(context_prompt),
        resume_session: existing_session,
        skip_permissions: true,
        ..Default::default()
    };

    let (response, session_id) = match claude::query_with_options(&text, options).await {
        Ok((response, session_id)) => (response, session_id),
        Err(e) => {
            warn!("Claude error: {}", e);
            (
                format!("Sorry, I encountered an error: {}", e),
                String::new(),
            )
        }
    };

    // Save session ID for future messages
    if !session_id.is_empty() {
        let key = format!("signal:{}", sender);
        if store.sessions.get(&key).map(|s| s.as_str()) != Some(&session_id) {
            store.sessions.insert(key, session_id);
            store.save()?;
        }
    }

    send_message(client, account, &sender, &response).await?;

    Ok(())
}

/// Handle onboarding flow
async fn handle_onboarding(message: &str) -> Result<String> {
    let system_prompt = onboarding::system_prompt()?;

    let options = claude::QueryOptions {
        system_prompt: Some(system_prompt),
        skip_permissions: true,
        ..Default::default()
    };

    let (response, _) = claude::query_with_options(message, options).await?;
    Ok(response)
}

/// Register a new Signal account (called during setup)
pub async fn register_account(phone_number: &str) -> Result<()> {
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

    let output = Command::new(&signal_cli)
        .args([
            "-a",
            phone_number,
            "--config",
            paths.signal_data_dir.to_str().unwrap(),
            "register",
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
        .context("Failed to run signal-cli register")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Registration failed: {}", stderr);
    }

    Ok(())
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
