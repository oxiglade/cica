//! Cursor CLI integration

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::setup;

#[cfg(target_os = "macos")]
const KEYCHAIN_PASSWORD: &str = "cica";

const DEFAULT_MODEL: &str = "opus-4.5";

pub const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("gpt-4o", "OpenAI GPT-4o"),
    ("auto", "Auto (let Cursor choose)"),
];

/// Falls back to `FALLBACK_MODELS` if the CLI is unavailable or the command fails.
pub async fn list_models(api_key: &str) -> Vec<(String, String)> {
    let cli = match setup::find_cursor_cli() {
        Some(p) => p,
        None => return fallback_models(),
    };

    let paths = match config::paths() {
        Ok(p) => p,
        Err(_) => return fallback_models(),
    };

    let output = Command::new(&cli)
        .args(["--list-models", "--api-key", api_key])
        .env("HOME", &paths.cursor_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return fallback_models(),
    };

    // Output format:
    //   Available models
    //   <blank>
    //   auto - Auto
    //   opus-4.6 - Claude 4.6 Opus  (current)
    //   opus-4.6-thinking - Claude 4.6 Opus (Thinking)  (default)
    //   ...
    //   <blank>
    //   Tip: use --model ...
    let stdout = String::from_utf8_lossy(&output.stdout);
    let models: Vec<(String, String)> = stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Available") || line.starts_with("Tip:") {
                return None;
            }
            let (id, rest) = line.split_once(" - ")?;
            let name = rest
                .trim_end_matches("(current)")
                .trim_end_matches("(default)")
                .trim();
            Some((id.trim().to_string(), name.to_string()))
        })
        .collect();

    if models.is_empty() {
        fallback_models()
    } else {
        models
    }
}

fn fallback_models() -> Vec<(String, String)> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

#[derive(Debug, Deserialize)]
struct CursorEvent {
    #[serde(rename = "type")]
    event_type: String,
    result: Option<String>,
    session_id: Option<String>,
    duration_ms: Option<u64>,
    is_error: Option<bool>,
}

#[derive(Default)]
pub struct QueryOptions {
    pub context: Option<String>,
    pub resume_session: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub force: bool,
}

#[allow(dead_code)]
pub async fn query(prompt: &str) -> Result<String> {
    let (result, _) = query_with_options(prompt, QueryOptions::default()).await?;
    Ok(result)
}

pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let config = Config::load()?;
    let paths = config::paths()?;

    let api_key = config.cursor.api_key.ok_or_else(|| {
        anyhow!("No Cursor API key configured. Run `cica init` to set up Cursor.")
    })?;

    let cursor_cli = setup::find_cursor_cli()
        .ok_or_else(|| anyhow!("Cursor CLI not found. Run `cica init` to set up Cursor."))?;

    let full_prompt = match &options.context {
        Some(context) => format!("<context>\n{}\n</context>\n\n{}", context, prompt),
        None => prompt.to_string(),
    };

    info!("Querying Cursor: {}", prompt);
    debug!("Using cursor_cli: {:?}", cursor_cli);

    ensure_keychain(&paths.cursor_home).await?;

    let mut cmd = Command::new(&cursor_cli);
    cmd.args(["-p", "--output-format", "stream-json"])
        .arg("--approve-mcps")
        .args(["--api-key", &api_key])
        .env("HOME", &paths.cursor_home);

    if options.force {
        cmd.arg("--force");
    }

    let model = options
        .model
        .or(config.cursor.model)
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    cmd.args(["--model", &model]);

    if let Some(ref session_id) = options.resume_session {
        cmd.args(["--resume", session_id]);
    }

    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&paths.base);
    }

    cmd.arg(&full_prompt);

    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        warn!("Cursor CLI failed. stdout: {}", stdout);
        warn!("Cursor CLI failed. stderr: {}", stderr);
        bail!(
            "Cursor CLI failed (exit {:?}): {}{}",
            output.status.code(),
            stderr,
            if stderr.is_empty() { &stdout } else { "" }
        );
    }

    debug!("Cursor raw output: {}", stdout);

    let mut final_result = None;
    let mut final_session_id = None;

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<CursorEvent>(line) else {
            continue;
        };

        if event.session_id.is_some() {
            final_session_id = event.session_id.clone();
        }

        if event.event_type == "result" {
            if event.is_error == Some(true) {
                bail!("Cursor returned an error");
            }
            if let Some(result) = event.result {
                info!(
                    "Cursor response received ({}ms)",
                    event.duration_ms.unwrap_or(0)
                );
                final_result = Some(result);
            }
        }
    }

    match final_result {
        Some(result) => Ok((result, final_session_id.unwrap_or_default())),
        None => Err(anyhow!("No result found in Cursor output")),
    }
}

#[cfg(target_os = "macos")]
async fn ensure_keychain(cursor_home: &Path) -> Result<()> {
    let keychain_dir = cursor_home.join("Library/Keychains");
    let keychain_path = keychain_dir.join("login.keychain-db");

    std::fs::create_dir_all(&keychain_dir)?;

    // Create keychain if it doesn't exist
    if !keychain_path.exists() {
        debug!("Creating sandboxed keychain at {:?}", keychain_path);
        let output = std::process::Command::new("security")
            .args([
                "create-keychain",
                "-p",
                KEYCHAIN_PASSWORD,
                keychain_path.to_str().unwrap(),
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "already exists" errors
            if !stderr.contains("already exists") {
                warn!("Failed to create keychain: {}", stderr);
            }
        }
    }

    // Unlock the keychain
    debug!("Unlocking sandboxed keychain");
    let output = std::process::Command::new("security")
        .args([
            "unlock-keychain",
            "-p",
            KEYCHAIN_PASSWORD,
            keychain_path.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Failed to unlock keychain: {}", stderr);
    }

    // Set keychain settings to not auto-lock
    let _ = std::process::Command::new("security")
        .args(["set-keychain-settings", keychain_path.to_str().unwrap()])
        .output();

    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn ensure_keychain(_cursor_home: &Path) -> Result<()> {
    Ok(())
}
