//! Claude Code integration

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::setup;

/// Response from Claude CLI in JSON format
#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    #[serde(rename = "type")]
    response_type: String,
    result: Option<String>,
    session_id: Option<String>,
    duration_ms: Option<u64>,
}

/// Options for querying Claude
#[derive(Default)]
pub struct QueryOptions {
    /// System prompt to use
    pub system_prompt: Option<String>,
    /// Session ID for conversation continuity
    pub session_id: Option<String>,
    /// Working directory for Claude
    pub cwd: Option<String>,
    /// Skip permission prompts (for automated flows)
    pub skip_permissions: bool,
}

/// Query Claude with a prompt and return the response
pub async fn query(prompt: &str) -> Result<String> {
    let (result, _) = query_with_options(prompt, QueryOptions::default()).await?;
    Ok(result)
}

/// Query Claude with options and return (response, session_id)
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
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

    info!("Querying Claude: {}", prompt);
    debug!("Using bun: {:?}", bun);
    debug!("Using claude_code: {:?}", claude_code);

    // Build command
    let mut cmd = Command::new(&bun);
    cmd.arg("run")
        .arg(&claude_code)
        .args(["-p", "--output-format", "json"])
        .env("HOME", &paths.claude_home);

    // Skip permissions if requested
    if options.skip_permissions {
        cmd.arg("--dangerously-skip-permissions");
    }

    // Add system prompt if provided
    if let Some(ref system_prompt) = options.system_prompt {
        cmd.args(["--system-prompt", system_prompt]);
    }

    // Add session ID if provided
    if let Some(ref session_id) = options.session_id {
        cmd.args(["--session-id", session_id]);
    }

    // Set working directory
    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&paths.base);
    }

    // Add the prompt
    cmd.arg(prompt);

    // Set auth env var based on credential type
    match setup::detect_credential_type(&credential) {
        setup::CredentialType::ApiKey => {
            cmd.env("ANTHROPIC_API_KEY", &credential);
        }
        setup::CredentialType::OAuthToken => {
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", &credential);
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

    // Parse the JSON response - find the result line
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
                    let session_id = response.session_id.unwrap_or_default();
                    return Ok((result, session_id));
                }
            }
        }
    }

    Err(anyhow!("No result found in Claude output"))
}
