//! Setup utilities for downloading and configuring Bun and Claude Code.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;

fn bun_download_url() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => {
            Ok("https://github.com/oven-sh/bun/releases/download/bun-v1.2.4/bun-darwin-aarch64.zip")
        }
        ("macos", "x86_64") => {
            Ok("https://github.com/oven-sh/bun/releases/download/bun-v1.2.4/bun-darwin-x64.zip")
        }
        ("linux", "aarch64") => {
            Ok("https://github.com/oven-sh/bun/releases/download/bun-v1.2.4/bun-linux-aarch64.zip")
        }
        ("linux", "x86_64") => {
            Ok("https://github.com/oven-sh/bun/releases/download/bun-v1.2.4/bun-linux-x64.zip")
        }
        (os, arch) => bail!("Unsupported platform: {}-{}", os, arch),
    }
}

/// Check if Bun is available (either system or bundled)
pub fn find_bun() -> Option<PathBuf> {
    // Check system bun first
    if let Ok(path) = which::which("bun") {
        return Some(path);
    }

    // Check our bundled bun
    if let Ok(paths) = config::paths() {
        let bundled = paths.bin_dir.join("bun");
        if bundled.exists() {
            return Some(bundled);
        }
    }

    None
}

/// Ensure Bun is available, downloading if necessary (async version)
pub async fn ensure_bun() -> Result<PathBuf> {
    // Check if already available
    if let Some(path) = find_bun() {
        return Ok(path);
    }

    // Need to download
    let paths = config::paths()?;
    std::fs::create_dir_all(&paths.bin_dir)?;

    let url = bun_download_url()?;
    let bun_path = paths.bin_dir.join("bun");

    download_and_extract_bun(url, &paths.bin_dir).await?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bun_path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(bun_path)
}

/// Download and extract Bun from a zip file (async)
async fn download_and_extract_bun(url: &str, dest_dir: &Path) -> Result<()> {
    // Download to memory
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("Failed to download Bun from {}", url))?;

    if !response.status().is_success() {
        bail!("Failed to download Bun: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    // Extract zip (sync, but on the downloaded bytes)
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

    // Find the bun binary in the archive (it's usually in a subdirectory)
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name();

        if name.ends_with("/bun") || name == "bun" {
            let dest_path = dest_dir.join("bun");
            let mut dest_file = std::fs::File::create(&dest_path)?;
            std::io::copy(&mut file, &mut dest_file)?;
            return Ok(());
        }
    }

    bail!("Could not find bun binary in archive")
}

/// Check if Claude Code is installed
pub fn find_claude_code() -> Option<PathBuf> {
    if let Ok(paths) = config::paths() {
        let entry = paths
            .claude_code_dir
            .join("node_modules/@anthropic-ai/claude-code/cli.js");
        if entry.exists() {
            return Some(entry);
        }
    }

    None
}

/// Ensure Claude Code is available, downloading if necessary
pub async fn ensure_claude_code() -> Result<PathBuf> {
    if let Some(path) = find_claude_code() {
        return Ok(path);
    }

    let paths = config::paths()?;
    std::fs::create_dir_all(&paths.claude_code_dir)?;

    // Use bun to install claude-code
    let bun = find_bun().ok_or_else(|| anyhow!("Bun not found - run ensure_bun first"))?;

    let status = tokio::process::Command::new(&bun)
        .args(["add", "@anthropic-ai/claude-code"])
        .current_dir(&paths.claude_code_dir)
        .status()
        .await
        .context("Failed to run bun add")?;

    if !status.success() {
        bail!("Failed to install Claude Code");
    }

    find_claude_code().ok_or_else(|| anyhow!("Claude Code installation failed"))
}

/// The type of credential
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialType {
    ApiKey,
    OAuthToken,
}

/// Detect the type of credential from its prefix
pub fn detect_credential_type(credential: &str) -> CredentialType {
    if credential.starts_with("sk-ant-oat") {
        CredentialType::OAuthToken
    } else {
        CredentialType::ApiKey
    }
}

/// Check if an OAuth token is set in environment
pub fn get_env_oauth_token() -> Option<String> {
    std::env::var("ANTHROPIC_OAUTH_TOKEN")
        .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
        .ok()
}

/// Minimum length for a valid setup token
const SETUP_TOKEN_MIN_LENGTH: usize = 80;

/// Validate a credential (API key or OAuth token)
pub async fn validate_credential(credential: &str) -> Result<()> {
    match detect_credential_type(credential) {
        CredentialType::ApiKey => validate_api_key(credential).await,
        CredentialType::OAuthToken => validate_oauth_token(credential),
    }
}

/// Validate an Anthropic API key
async fn validate_api_key(api_key: &str) -> Result<()> {
    let client = reqwest::Client::new();

    let response = client
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("Failed to connect to Anthropic API")?;

    if response.status().is_success() {
        Ok(())
    } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Invalid API key")
    } else {
        bail!("API error: {}", response.status())
    }
}

/// Validate an OAuth/setup token by checking its format
/// Setup tokens may not have scopes to call API endpoints, so we just validate format
fn validate_oauth_token(token: &str) -> Result<()> {
    let trimmed = token.trim();

    if !trimmed.starts_with("sk-ant-oat") {
        bail!("Invalid token format: expected token starting with sk-ant-oat");
    }

    if trimmed.len() < SETUP_TOKEN_MIN_LENGTH {
        bail!(
            "Token looks too short (got {} chars, expected at least {}). Paste the full setup token.",
            trimmed.len(),
            SETUP_TOKEN_MIN_LENGTH
        );
    }

    Ok(())
}
