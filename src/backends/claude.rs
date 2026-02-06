//! Claude Code integration

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::setup;

pub const MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
];

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    #[serde(rename = "type")]
    response_type: String,
    result: Option<String>,
    session_id: Option<String>,
    duration_ms: Option<u64>,
}

#[derive(Default)]
pub struct QueryOptions {
    pub system_prompt: Option<String>,
    pub resume_session: Option<String>,
    pub cwd: Option<String>,
    pub skip_permissions: bool,
    /// Model alias ("sonnet", "opus") or full model ID (e.g. "claude-sonnet-4-5-20250929")
    pub model: Option<String>,
}

#[allow(dead_code)]
pub async fn query(prompt: &str) -> Result<String> {
    let (result, _) = query_with_options(prompt, QueryOptions::default()).await?;
    Ok(result)
}

pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let config = Config::load()?;
    let paths = config::paths()?;

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

    let bun = setup::find_bun()
        .ok_or_else(|| anyhow!("Bun not found. Run `cica init` to set up Claude."))?;

    let claude_code = setup::find_claude_code()
        .ok_or_else(|| anyhow!("Claude Code not found. Run `cica init` to set up Claude."))?;

    info!("Querying Claude: {}", prompt);
    debug!("Using bun: {:?}", bun);
    debug!("Using claude_code: {:?}", claude_code);

    let mut cmd = Command::new(&bun);
    cmd.arg("run")
        .arg(&claude_code)
        .args(["-p", "--output-format", "json"])
        .env("HOME", &paths.claude_home);

    if options.skip_permissions {
        cmd.arg("--dangerously-skip-permissions");
    }

    if let Some(ref system_prompt) = options.system_prompt {
        if options.resume_session.is_none() {
            // New session: full system prompt
            cmd.args(["--system-prompt", system_prompt]);
        } else {
            // Resuming: append as reminder
            cmd.args(["--append-system-prompt", system_prompt]);
        }
    }

    if let Some(ref session_id) = options.resume_session {
        cmd.args(["--resume", session_id]);
    }

    if let Some(ref model) = options.model {
        cmd.args(["--model", model]);
    }

    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&paths.base);
    }

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
