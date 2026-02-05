//! Setup utilities for downloading and configuring Bun, Claude Code, Java, signal-cli, and embedding models.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

use crate::config;
use crate::memory;

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

const SIGNAL_CLI_VERSION: &str = "0.13.22";

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

// ============================================================================
// Cursor CLI
// ============================================================================

/// Cursor CLI version to download
const CURSOR_CLI_VERSION: &str = "2026.01.28-fd13201";

/// Check if Cursor CLI is available
pub fn find_cursor_cli() -> Option<PathBuf> {
    // Check our bundled cursor-cli first
    if let Ok(paths) = config::paths() {
        let bundled = paths.cursor_cli_dir.join("cursor-agent");
        if bundled.exists() {
            return Some(bundled);
        }
    }

    // Check system agent (user might have installed it globally)
    // Cursor installs as both "agent" and "cursor-agent"
    if let Ok(path) = which::which("cursor-agent") {
        return Some(path);
    }
    if let Ok(path) = which::which("agent") {
        return Some(path);
    }

    None
}

/// Ensure Cursor CLI is available, downloading if necessary
pub async fn ensure_cursor_cli() -> Result<PathBuf> {
    if let Some(path) = find_cursor_cli() {
        return Ok(path);
    }

    let paths = config::paths()?;
    std::fs::create_dir_all(&paths.cursor_cli_dir)?;
    std::fs::create_dir_all(&paths.cursor_home)?;

    // Download Cursor CLI
    download_cursor_cli(&paths.cursor_cli_dir).await?;

    find_cursor_cli().ok_or_else(|| anyhow!("Cursor CLI installation failed"))
}

/// Download and extract Cursor CLI from tarball
async fn download_cursor_cli(dest_dir: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let url = cursor_cli_download_url()?;

    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("Failed to download Cursor CLI from {}", url))?;

    if !response.status().is_success() {
        bail!("Failed to download Cursor CLI: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    // Extract tarball with --strip-components=1 equivalent
    // The tarball contains dist-package/cursor-agent, we want cursor-agent directly
    let cursor = std::io::Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);

    // Extract entries, stripping the first path component (dist-package/)
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;

        // Strip first component (dist-package/)
        let stripped: PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }

        let dest_path = dest_dir.join(&stripped);

        // Create parent directories if needed
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Unpack the entry
        entry.unpack(&dest_path)?;
    }

    // The binary should be at dest_dir/cursor-agent after extraction
    let agent_path = dest_dir.join("cursor-agent");

    // Make executable
    #[cfg(unix)]
    if agent_path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent_path, std::fs::Permissions::from_mode(0o755))?;
    }

    if !agent_path.exists() {
        bail!("Could not find cursor-agent binary in downloaded archive");
    }

    Ok(())
}

/// Get the Cursor CLI download URL for the current platform
fn cursor_cli_download_url() -> Result<String> {
    // URL pattern: https://downloads.cursor.com/lab/{VERSION}/{OS}/{ARCH}/agent-cli-package.tar.gz
    let (os, arch) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin", "arm64"),
        ("macos", "x86_64") => ("darwin", "x64"),
        ("linux", "aarch64") => ("linux", "arm64"),
        ("linux", "x86_64") => ("linux", "x64"),
        (os, arch) => bail!("Unsupported platform for Cursor CLI: {}-{}", os, arch),
    };

    Ok(format!(
        "https://downloads.cursor.com/lab/{}/{}/{}/agent-cli-package.tar.gz",
        CURSOR_CLI_VERSION, os, arch
    ))
}

/// Validate a GCP service account JSON key file (exists and has required fields).
/// Use this for long-lived auth on servers; the key does not expire like gcloud login.
pub fn validate_vertex_credentials_path(path: &str, base_dir: &Path) -> Result<()> {
    let p = Path::new(path.trim());
    let full = if p.is_relative() {
        base_dir.join(p)
    } else {
        p.to_path_buf()
    };
    if !full.exists() {
        bail!("Credentials file not found: {}", full.display());
    }
    let content = std::fs::read_to_string(&full)
        .with_context(|| format!("Could not read credentials file: {}", full.display()))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Invalid JSON in credentials file: {}", full.display()))?;
    let obj = json
        .as_object()
        .ok_or_else(|| anyhow!("Credentials file must be a JSON object"))?;
    if !obj.contains_key("client_email") || !obj.contains_key("private_key") {
        bail!(
            "Credentials file must contain \"client_email\" and \"private_key\" (GCP service account key)"
        );
    }
    Ok(())
}

/// Validate Vertex AI configuration (project ID set and GCP auth available).
/// If credentials_path is Some, validates that file and does not require gcloud.
pub async fn validate_vertex_config(
    project_id: &str,
    _region: Option<&str>,
    credentials_path: Option<&str>,
    base_dir: &Path,
) -> Result<()> {
    let trimmed = project_id.trim();
    if trimmed.is_empty() {
        bail!("Vertex AI project ID cannot be empty");
    }
    if let Some(path) = credentials_path {
        let p = path.trim();
        if !p.is_empty() {
            validate_vertex_credentials_path(p, base_dir)?;
            return Ok(());
        }
    }
    // No key file: check gcloud ADC or GOOGLE_APPLICATION_CREDENTIALS
    let check = tokio::process::Command::new("gcloud")
        .args(["auth", "application-default", "print-access-token"])
        .output()
        .await;
    match check {
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) => bail!(
            "GCP credentials not found. Run: gcloud auth application-default login \
             or set a service account key path in cica init (recommended for servers)"
        ),
        Err(_) => {
            if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok() {
                Ok(())
            } else {
                bail!(
                    "Neither gcloud nor GOOGLE_APPLICATION_CREDENTIALS found. \
                     For Vertex AI, run: gcloud auth application-default login \
                     or set a service account key path in cica init (recommended for servers)"
                )
            }
        }
    }
}

/// Validate a Cursor API key by making a test request
pub async fn validate_cursor_api_key(api_key: &str) -> Result<()> {
    // Cursor uses their own API - we can try to list models to validate
    // For now, just do basic format validation
    let trimmed = api_key.trim();

    if trimmed.is_empty() {
        bail!("API key cannot be empty");
    }

    // Cursor API keys typically have a specific format
    // For now, accept any non-empty string since we don't know the exact format
    // The real validation will happen when we try to use it

    Ok(())
}

// ============================================================================
// Embedding Model (for memory search)
// ============================================================================

/// Ensure the embedding model is downloaded
pub fn ensure_embedding_model() -> Result<()> {
    memory::ensure_model_downloaded()
}
