//! Setup utilities for downloading and configuring Bun, Claude Code, Java, and signal-cli.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

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
        let bundled = paths.bun_dir.join("bun");
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
    std::fs::create_dir_all(&paths.bun_dir)?;

    let url = bun_download_url()?;
    let bun_path = paths.bun_dir.join("bun");

    download_and_extract_bun(url, &paths.bun_dir).await?;

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

// ============================================================================
// Java (for signal-cli)
// ============================================================================

const SIGNAL_CLI_VERSION: &str = "0.13.12";

fn java_download_url() -> Result<&'static str> {
    // Eclipse Temurin JRE 21 from Adoptium
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(
            "https://api.adoptium.net/v3/binary/latest/21/ga/mac/aarch64/jre/hotspot/normal/eclipse",
        ),
        ("macos", "x86_64") => {
            Ok("https://api.adoptium.net/v3/binary/latest/21/ga/mac/x64/jre/hotspot/normal/eclipse")
        }
        ("linux", "aarch64") => Ok(
            "https://api.adoptium.net/v3/binary/latest/21/ga/linux/aarch64/jre/hotspot/normal/eclipse",
        ),
        ("linux", "x86_64") => Ok(
            "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jre/hotspot/normal/eclipse",
        ),
        (os, arch) => bail!("Unsupported platform for Java: {}-{}", os, arch),
    }
}

fn signal_cli_download_url() -> String {
    format!(
        "https://github.com/AsamK/signal-cli/releases/download/v{}/signal-cli-{}.tar.gz",
        SIGNAL_CLI_VERSION, SIGNAL_CLI_VERSION
    )
}

/// Check if Java is available (bundled only - we don't use system Java)
pub fn find_java() -> Option<PathBuf> {
    let paths = config::paths().ok()?;
    let entries = std::fs::read_dir(&paths.java_dir).ok()?;

    for entry in entries.flatten() {
        let base = entry.path();

        #[cfg(target_os = "linux")]
        let java_path = base.join("bin").join("java");

        #[cfg(target_os = "macos")]
        let java_path = base.join("Contents").join("Home").join("bin").join("java");

        if java_path.exists() {
            return Some(java_path);
        }
    }

    None
}

/// Ensure Java is available, downloading if necessary
pub async fn ensure_java() -> Result<PathBuf> {
    if let Some(path) = find_java() {
        return Ok(path);
    }

    let paths = config::paths()?;
    std::fs::create_dir_all(&paths.java_dir)?;

    let url = java_download_url()?;
    download_and_extract_tarball(url, &paths.java_dir).await?;

    find_java()
        .ok_or_else(|| anyhow!("Java installation failed - binary not found after extraction"))
}

/// Check if signal-cli is available
pub fn find_signal_cli() -> Option<PathBuf> {
    if let Ok(paths) = config::paths() {
        // Look for signal-cli script
        let direct = paths.signal_cli_dir.join("bin").join("signal-cli");
        if direct.exists() {
            return Some(direct);
        }

        // Check for extracted directory structure (e.g., signal-cli-0.13.12/bin/signal-cli)
        if let Ok(entries) = std::fs::read_dir(&paths.signal_cli_dir) {
            for entry in entries.flatten() {
                let cli_path = entry.path().join("bin").join("signal-cli");
                if cli_path.exists() {
                    return Some(cli_path);
                }
            }
        }
    }

    None
}

/// Ensure signal-cli is available, downloading if necessary
pub async fn ensure_signal_cli() -> Result<PathBuf> {
    if let Some(path) = find_signal_cli() {
        return Ok(path);
    }

    let paths = config::paths()?;
    std::fs::create_dir_all(&paths.signal_cli_dir)?;

    let url = signal_cli_download_url();
    download_and_extract_tarball(&url, &paths.signal_cli_dir).await?;

    find_signal_cli().ok_or_else(|| {
        anyhow!("signal-cli installation failed - binary not found after extraction")
    })
}

/// Download and extract a tarball (.tar.gz)
async fn download_and_extract_tarball(url: &str, dest_dir: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("Failed to download from {}", url))?;

    if !response.status().is_success() {
        bail!("Failed to download: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    // Extract tarball
    let cursor = std::io::Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);
    archive.unpack(dest_dir)?;

    Ok(())
}
