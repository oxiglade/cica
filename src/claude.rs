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
    /// Resume an existing session by ID (uses --resume)
    pub resume_session: Option<String>,
    /// Working directory for Claude
    pub cwd: Option<String>,
    /// Skip permission prompts (for automated flows)
    pub skip_permissions: bool,
}

/// Query Claude with a prompt and return the response
#[allow(dead_code)]
pub async fn query(prompt: &str) -> Result<String> {
    let (result, _) = query_with_options(prompt, QueryOptions::default()).await?;
    Ok(result)
}

/// Query Claude with options and return (response, session_id)
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let config = Config::load()?;
    let paths = config::paths()?;

    // Resolve credential or Vertex config
    let use_vertex = config.claude.use_vertex;
    let vertex_project_id = config.claude.vertex_project_id.as_deref();
    let credential = config.claude.api_key.as_deref();

    if use_vertex {
        let project_id = vertex_project_id
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Vertex AI is enabled but no project ID is set. Run `cica init` to configure Vertex AI."))?;
        debug!("Using Vertex AI project: {}", project_id);
    } else {
        credential.ok_or_else(|| {
            anyhow!("No credential configured. Run `cica init` to set up Claude.")
        })?;
    }

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

    // Add system prompt
    if let Some(ref system_prompt) = options.system_prompt {
        if options.resume_session.is_none() {
            // New session: full system prompt
            cmd.args(["--system-prompt", system_prompt]);
        } else {
            // Resuming: append as reminder
            cmd.args(["--append-system-prompt", system_prompt]);
        }
    }

    // Resume existing session if provided
    if let Some(ref session_id) = options.resume_session {
        cmd.args(["--resume", session_id]);
    }

    // Set working directory
    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&paths.base);
    }

    // Add the prompt
    cmd.arg(prompt);

    // Set auth env vars: either Vertex AI (GCP) or Anthropic API key / OAuth
    if use_vertex {
        cmd.env("CLAUDE_CODE_USE_VERTEX", "1");
        cmd.env(
            "ANTHROPIC_VERTEX_PROJECT_ID",
            vertex_project_id.unwrap_or(""),
        );
        cmd.env(
            "CLOUD_ML_REGION",
            config
                .claude
                .vertex_region
                .as_deref()
                .unwrap_or("europe-west1"),
        );
        // Long-lived auth: service account key file (recommended for servers; no gcloud expiry)
        if let Some(ref cred_path) = config.claude.vertex_credentials_path {
            let path = std::path::Path::new(cred_path);
            let abs = if path.is_relative() {
                paths.base.join(cred_path)
            } else {
                path.to_path_buf()
            };
            if abs.exists() {
                cmd.env("GOOGLE_APPLICATION_CREDENTIALS", &abs);
            }
        }
        // Otherwise Vertex uses gcloud ADC or existing GOOGLE_APPLICATION_CREDENTIALS env
    } else if let Some(cred) = credential {
        match setup::detect_credential_type(cred) {
            setup::CredentialType::ApiKey => {
                cmd.env("ANTHROPIC_API_KEY", cred);
            }
            setup::CredentialType::OAuthToken => {
                cmd.env("CLAUDE_CODE_OAUTH_TOKEN", cred);
                cmd.env("ANTHROPIC_OAUTH_TOKEN", cred);
            }
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

        let Ok(response) = serde_json::from_str::<ClaudeResponse>(line) else {
            continue;
        };

        if response.response_type == "result"
            && let Some(result) = response.result
        {
            info!(
                "Claude response received ({}ms)",
                response.duration_ms.unwrap_or(0)
            );
            let session_id = response.session_id.unwrap_or_default();
            return Ok((result, session_id));
        }
    }

    Err(anyhow!("No result found in Claude output"))
}
