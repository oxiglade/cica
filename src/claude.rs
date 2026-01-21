use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::setup;

/// Response from Claude CLI in JSON format
#[derive(Debug, Deserialize)]
pub struct ClaudeResponse {
    #[serde(rename = "type")]
    pub response_type: String,
    pub subtype: Option<String>,
    pub cost_usd: Option<f64>,
    pub is_error: Option<bool>,
    pub duration_ms: Option<u64>,
    pub duration_api_ms: Option<u64>,
    pub num_turns: Option<u32>,
    pub result: Option<String>,
    pub session_id: Option<String>,
}

/// Query Claude with a prompt and return the response
pub async fn query(prompt: &str) -> Result<String> {
    let config = Config::load()?;
    let paths = config::paths()?;

    // Get credential
    let credential = config
        .claude
        .api_key
        .ok_or_else(|| anyhow!("No credential configured. Run `cica init` to set up Claude."))?;

    // Get Bun path
    let bun = setup::find_bun()
        .ok_or_else(|| anyhow!("Bun not found. Run `cica init` to set up Claude."))?;

    // Get Claude Code entry point
    let claude_code = setup::find_claude_code()
        .ok_or_else(|| anyhow!("Claude Code not found. Run `cica init` to set up Claude."))?;

    info!("Querying Claude with prompt: {}", prompt);
    debug!("Using bun: {:?}", bun);
    debug!("Using claude_code: {:?}", claude_code);
    debug!("Using HOME: {:?}", paths.claude_home);
    debug!(
        "Credential type: {:?}",
        setup::detect_credential_type(&credential)
    );

    // Build command with appropriate auth env var
    let mut cmd = Command::new(&bun);
    cmd.arg("run")
        .arg(&claude_code)
        .args(["-p", "--output-format", "json"])
        .arg(prompt)
        .env("HOME", &paths.claude_home);

    // Set the right env var based on credential type
    // Try both env vars that Claude Code might look for
    match setup::detect_credential_type(&credential) {
        setup::CredentialType::ApiKey => {
            cmd.env("ANTHROPIC_API_KEY", &credential);
        }
        setup::CredentialType::OAuthToken => {
            // Claude Code docs say CLAUDE_CODE_OAUTH_TOKEN
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", &credential);
            // But also try ANTHROPIC_OAUTH_TOKEN just in case
            cmd.env("ANTHROPIC_OAUTH_TOKEN", &credential);
        }
    }

    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        warn!("Claude CLI failed. stdout: {}", stdout);
        warn!("Claude CLI failed. stderr: {}", stderr);
        bail!(
            "Claude CLI failed (exit {:?}): {}{}",
            output.status.code(),
            stderr,
            if stderr.is_empty() { &stdout } else { "" }
        );
    }

    debug!("Claude raw output: {}", stdout);

    // Parse the JSON response
    // Claude outputs multiple JSON lines, we want the final "result" type
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(response) = serde_json::from_str::<ClaudeResponse>(line) {
            if response.response_type == "result" {
                if let Some(result) = response.result {
                    info!(
                        "Claude response received ({}ms)",
                        response.duration_ms.unwrap_or(0)
                    );
                    return Ok(result);
                }
            }
        }
    }

    Err(anyhow!("No result found in Claude output"))
}

/// Query Claude with a session ID for context continuity
pub async fn query_with_session(prompt: &str, session_id: &str) -> Result<(String, String)> {
    let config = Config::load()?;
    let paths = config::paths()?;

    let credential = config
        .claude
        .api_key
        .ok_or_else(|| anyhow!("No credential configured. Run `cica init` to set up Claude."))?;

    let bun = setup::find_bun()
        .ok_or_else(|| anyhow!("Bun not found. Run `cica init` to set up Claude."))?;

    let claude_code = setup::find_claude_code()
        .ok_or_else(|| anyhow!("Claude Code not found. Run `cica init` to set up Claude."))?;

    info!("Querying Claude with session {}: {}", session_id, prompt);

    let mut cmd = Command::new(&bun);
    cmd.arg("run")
        .arg(&claude_code)
        .args(["-p", "--output-format", "json", "--session-id", session_id])
        .arg(prompt)
        .env("HOME", &paths.claude_home);

    match setup::detect_credential_type(&credential) {
        setup::CredentialType::ApiKey => {
            cmd.env("ANTHROPIC_API_KEY", &credential);
        }
        setup::CredentialType::OAuthToken => {
            cmd.env("ANTHROPIC_OAUTH_TOKEN", &credential);
        }
    }

    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Claude CLI failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(response) = serde_json::from_str::<ClaudeResponse>(line) {
            if response.response_type == "result" {
                if let Some(result) = response.result {
                    let new_session_id = response
                        .session_id
                        .unwrap_or_else(|| session_id.to_string());
                    return Ok((result, new_session_id));
                }
            }
        }
    }

    Err(anyhow!("No result found in Claude output"))
}
