//! Setup utilities for downloading and configuring Bun, Claude Code, Java, signal-cli, and embedding models.

use anyhow::{Context, Result, anyhow, bail};
use semver::{Version, VersionReq};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{info, warn};

use crate::config::Paths;
use crate::memory;

// ============================================================================
// Pinned Versions
// ============================================================================

const BUN_VERSION: &str = "1.2.4";
// Semver range; bun resolves it. Keep in sync with the Dockerfile's CLAUDE_CODE_VERSION ARG.
const CLAUDE_CODE_VERSION: &str = "^2.1.258";

const VERSION_FILE: &str = ".version";

fn read_installed_version(dep_dir: &Path) -> Option<String> {
    std::fs::read_to_string(dep_dir.join(VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_installed_version(dep_dir: &Path, version: &str) -> Result<()> {
    std::fs::write(dep_dir.join(VERSION_FILE), version)?;
    Ok(())
}

fn needs_update(dep_dir: &Path, expected: &str) -> bool {
    read_installed_version(dep_dir).as_deref() != Some(expected)
}

fn read_claude_code_manifest_version(dep_dir: &Path) -> Option<String> {
    let manifest = dep_dir.join("node_modules/@anthropic-ai/claude-code/package.json");
    let raw = std::fs::read_to_string(manifest).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed.get("version")?.as_str().map(str::to_string)
}

fn installed_claude_code_version(dep_dir: &Path) -> Option<Version> {
    read_installed_version(dep_dir)
        .and_then(|v| Version::parse(&v).ok())
        // Installs baked into the container image write no `.version`.
        .or_else(|| {
            read_claude_code_manifest_version(dep_dir).and_then(|v| Version::parse(&v).ok())
        })
}

// ============================================================================
// Bun
// ============================================================================

fn bun_download_url() -> Result<String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(format!(
            "https://github.com/oven-sh/bun/releases/download/bun-v{}/bun-darwin-aarch64.zip",
            BUN_VERSION
        )),
        ("macos", "x86_64") => Ok(format!(
            "https://github.com/oven-sh/bun/releases/download/bun-v{}/bun-darwin-x64.zip",
            BUN_VERSION
        )),
        ("linux", "aarch64") => Ok(format!(
            "https://github.com/oven-sh/bun/releases/download/bun-v{}/bun-linux-aarch64.zip",
            BUN_VERSION
        )),
        ("linux", "x86_64") => Ok(format!(
            "https://github.com/oven-sh/bun/releases/download/bun-v{}/bun-linux-x64.zip",
            BUN_VERSION
        )),
        (os, arch) => bail!("Unsupported platform: {}-{}", os, arch),
    }
}

/// Check if Bun is available (either system or bundled)
pub fn find_bun(paths: &Paths) -> Option<PathBuf> {
    if let Ok(path) = which::which("bun") {
        return Some(path);
    }

    let bundled = paths.bun_dir.join("bun");
    if bundled.exists() {
        return Some(bundled);
    }

    None
}

pub async fn ensure_bun(paths: &Paths) -> Result<PathBuf> {
    if find_bun(paths).is_some() && !needs_update(&paths.bun_dir, BUN_VERSION) {
        return find_bun(paths).ok_or_else(|| anyhow!("Bun not found"));
    }

    if needs_update(&paths.bun_dir, BUN_VERSION) {
        info!("Updating Bun to v{}...", BUN_VERSION);
        let _ = std::fs::remove_dir_all(&paths.bun_dir);
    }

    std::fs::create_dir_all(&paths.bun_dir)?;

    let url = bun_download_url()?;
    let bun_path = paths.bun_dir.join("bun");

    download_and_extract_bun(&url, &paths.bun_dir).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bun_path, std::fs::Permissions::from_mode(0o755))?;
    }

    write_installed_version(&paths.bun_dir, BUN_VERSION)?;
    Ok(bun_path)
}

async fn download_and_extract_bun(url: &str, dest_dir: &Path) -> Result<()> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("Failed to download Bun from {}", url))?;

    if !response.status().is_success() {
        bail!("Failed to download Bun: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

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

/// Up to 2.1.112 the npm package was JavaScript with a `cli.js` run under bun.
/// From 2.1.113 it is a native binary the installer links into
/// `node_modules/.bin/claude`; the package's own `bin/claude.exe` is a
/// non-executable stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCode {
    Native(PathBuf),
    Script(PathBuf),
}

/// Without postinstall, `.bin/claude` can target a shebang-less stub while the platform binary is installed; legitimate text wrappers still need `#!`.
fn runnable_entry(path: &Path, os: &str) -> Option<ClaudeCode> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 1024];
    let read = file.read(&mut head).ok()?;
    let head = &head[..read];
    if head.starts_with(b"#!") {
        let line = String::from_utf8_lossy(head.split(|byte| *byte == b'\n').next()?);
        let javascript = std::fs::canonicalize(path)
            .ok()
            .and_then(|target| {
                target
                    .extension()
                    .map(|ext| matches!(ext.to_str(), Some("js" | "mjs" | "cjs")))
            })
            .unwrap_or(false)
            || line.split_whitespace().any(|word| {
                matches!(
                    Path::new(word).file_name().and_then(|name| name.to_str()),
                    Some("node" | "nodejs" | "bun")
                )
            });
        return Some(if javascript {
            ClaudeCode::Script(path.to_path_buf())
        } else {
            ClaudeCode::Native(path.to_path_buf())
        });
    }
    let native = match os {
        "linux" => head.starts_with(b"\x7fELF"),
        "macos" => [
            b"\xfe\xed\xfa\xce",
            b"\xce\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xcf\xfa\xed\xfe",
            b"\xca\xfe\xba\xbe",
            b"\xbe\xba\xfe\xca",
            b"\xca\xfe\xba\xbf",
            b"\xbf\xba\xfe\xca",
        ]
        .iter()
        .any(|magic| head.starts_with(*magic)),
        _ => false,
    };
    native.then(|| ClaudeCode::Native(path.to_path_buf()))
}

pub fn find_claude_code(paths: &Paths) -> Option<ClaudeCode> {
    find_claude_code_for_host(paths, std::env::consts::OS, std::env::consts::ARCH)
}

fn find_claude_code_for_host(paths: &Paths, os: &str, arch: &str) -> Option<ClaudeCode> {
    let modules = paths.claude_code_dir.join("node_modules");

    if let Some(entry) = runnable_entry(&modules.join(".bin/claude"), os) {
        return Some(entry);
    }

    let scoped = modules.join("@anthropic-ai");
    let platform = match (os, arch) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        _ => None,
    };
    if let Some(platform) = platform {
        let suffix = if os == "linux" && cfg!(target_env = "musl") {
            "-musl"
        } else {
            ""
        };
        let candidate = scoped.join(format!("claude-code-{platform}{suffix}/claude"));
        if let Some(entry) = runnable_entry(&candidate, os) {
            return Some(entry);
        }
    }

    let script = scoped.join("claude-code/cli.js");
    if script.is_file() {
        return Some(ClaudeCode::Script(script));
    }

    None
}

pub async fn ensure_claude_code(paths: &Paths) -> Result<ClaudeCode> {
    let req = VersionReq::parse(CLAUDE_CODE_VERSION).with_context(|| {
        format!(
            "Invalid Claude Code version requirement: {}",
            CLAUDE_CODE_VERSION
        )
    })?;

    if let Some(entry) = find_claude_code(paths) {
        match installed_claude_code_version(&paths.claude_code_dir) {
            Some(installed) if req.matches(&installed) => {
                let resolved = installed.to_string();
                if read_installed_version(&paths.claude_code_dir).as_deref() != Some(&resolved) {
                    let _ = write_installed_version(&paths.claude_code_dir, &resolved);
                }
                return Ok(entry);
            }
            Some(installed) => info!(
                "Claude Code v{} no longer satisfies {} - reinstalling...",
                installed, CLAUDE_CODE_VERSION
            ),
            None => info!("Claude Code version could not be determined - reinstalling..."),
        }

        let _ = std::fs::remove_dir_all(&paths.claude_code_dir);
    }

    std::fs::create_dir_all(&paths.claude_code_dir)?;

    let bun = find_bun(paths).ok_or_else(|| anyhow!("Bun not found - run ensure_bun first"))?;
    let pkg = format!("@anthropic-ai/claude-code@{}", CLAUDE_CODE_VERSION);

    info!("Installing Claude Code {}...", CLAUDE_CODE_VERSION);

    let status = tokio::process::Command::new(&bun)
        .args(["add", &pkg])
        .current_dir(&paths.claude_code_dir)
        .status()
        .await
        .context("Failed to run bun add")?;

    if !status.success() {
        bail!("Failed to install Claude Code");
    }

    let entry =
        find_claude_code(paths).ok_or_else(|| anyhow!("Claude Code installation failed"))?;

    let resolved = read_claude_code_manifest_version(&paths.claude_code_dir)
        .ok_or_else(|| anyhow!("Could not read the installed Claude Code version"))?;
    let resolved_version = Version::parse(&resolved)
        .with_context(|| format!("Claude Code reported an unparseable version: {}", resolved))?;

    if !req.matches(&resolved_version) {
        bail!(
            "bun installed Claude Code v{} which does not satisfy {}",
            resolved,
            CLAUDE_CODE_VERSION
        );
    }

    info!("Claude Code v{} installed", resolved);
    write_installed_version(&paths.claude_code_dir, &resolved)?;
    Ok(entry)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialType {
    ApiKey,
    OAuthToken,
}

pub fn detect_credential_type(credential: &str) -> CredentialType {
    if credential.starts_with("sk-ant-oat") {
        CredentialType::OAuthToken
    } else {
        CredentialType::ApiKey
    }
}

pub fn get_env_oauth_token() -> Option<String> {
    std::env::var("ANTHROPIC_OAUTH_TOKEN")
        .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
        .ok()
}

const SETUP_TOKEN_MIN_LENGTH: usize = 80;

pub async fn validate_credential(credential: &str) -> Result<()> {
    match detect_credential_type(credential) {
        CredentialType::ApiKey => validate_api_key(credential).await,
        CredentialType::OAuthToken => validate_oauth_token(credential),
    }
}

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
// Java & signal-cli
// ============================================================================

const JAVA_VERSION: &str = "21";
const SIGNAL_CLI_VERSION: &str = "0.13.22";

fn java_download_url() -> Result<&'static str> {
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

/// Bundled only — we don't use system Java.
pub fn find_java(paths: &Paths) -> Option<PathBuf> {
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

pub async fn ensure_java(paths: &Paths) -> Result<PathBuf> {
    if find_java(paths).is_some() && !needs_update(&paths.java_dir, JAVA_VERSION) {
        return find_java(paths).ok_or_else(|| anyhow!("Java not found"));
    }

    if needs_update(&paths.java_dir, JAVA_VERSION) {
        info!("Updating Java JRE {}...", JAVA_VERSION);
        let _ = std::fs::remove_dir_all(&paths.java_dir);
    }

    std::fs::create_dir_all(&paths.java_dir)?;

    let url = java_download_url()?;
    download_and_extract_tarball(url, &paths.java_dir).await?;

    write_installed_version(&paths.java_dir, JAVA_VERSION)?;
    find_java(paths)
        .ok_or_else(|| anyhow!("Java installation failed - binary not found after extraction"))
}

pub fn find_signal_cli(paths: &Paths) -> Option<PathBuf> {
    let direct = paths.signal_cli_dir.join("bin").join("signal-cli");
    if direct.exists() {
        return Some(direct);
    }

    if let Ok(entries) = std::fs::read_dir(&paths.signal_cli_dir) {
        for entry in entries.flatten() {
            let cli_path = entry.path().join("bin").join("signal-cli");
            if cli_path.exists() {
                return Some(cli_path);
            }
        }
    }

    None
}

pub async fn ensure_signal_cli(paths: &Paths) -> Result<PathBuf> {
    if find_signal_cli(paths).is_some() && !needs_update(&paths.signal_cli_dir, SIGNAL_CLI_VERSION)
    {
        return find_signal_cli(paths).ok_or_else(|| anyhow!("signal-cli not found"));
    }

    if needs_update(&paths.signal_cli_dir, SIGNAL_CLI_VERSION) {
        info!("Updating signal-cli to v{}...", SIGNAL_CLI_VERSION);
        let _ = std::fs::remove_dir_all(&paths.signal_cli_dir);
    }

    std::fs::create_dir_all(&paths.signal_cli_dir)?;

    let url = signal_cli_download_url();
    download_and_extract_tarball(&url, &paths.signal_cli_dir).await?;

    write_installed_version(&paths.signal_cli_dir, SIGNAL_CLI_VERSION)?;
    find_signal_cli(paths).ok_or_else(|| {
        anyhow!("signal-cli installation failed - binary not found after extraction")
    })
}

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

    let cursor = std::io::Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);
    archive.unpack(dest_dir)?;

    Ok(())
}

// ============================================================================
// Cursor CLI
// ============================================================================

const CURSOR_CLI_VERSION: &str = "2026.01.28-fd13201";

pub fn find_cursor_cli(paths: &Paths) -> Option<PathBuf> {
    let bundled = paths.cursor_cli_dir.join("cursor-agent");
    if bundled.exists() {
        return Some(bundled);
    }

    // Cursor installs as both "agent" and "cursor-agent"
    if let Ok(path) = which::which("cursor-agent") {
        return Some(path);
    }
    if let Ok(path) = which::which("agent") {
        return Some(path);
    }

    None
}

pub async fn ensure_cursor_cli(paths: &Paths) -> Result<PathBuf> {
    if find_cursor_cli(paths).is_some() && !needs_update(&paths.cursor_cli_dir, CURSOR_CLI_VERSION)
    {
        return find_cursor_cli(paths).ok_or_else(|| anyhow!("Cursor CLI not found"));
    }

    if needs_update(&paths.cursor_cli_dir, CURSOR_CLI_VERSION) {
        info!("Updating Cursor CLI to {}...", CURSOR_CLI_VERSION);
        let _ = std::fs::remove_dir_all(&paths.cursor_cli_dir);
    }

    std::fs::create_dir_all(&paths.cursor_cli_dir)?;
    std::fs::create_dir_all(&paths.cursor_home)?;

    download_cursor_cli(&paths.cursor_cli_dir).await?;

    write_installed_version(&paths.cursor_cli_dir, CURSOR_CLI_VERSION)?;
    find_cursor_cli(paths).ok_or_else(|| anyhow!("Cursor CLI installation failed"))
}

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

    // Strip the leading dist-package/ component (--strip-components=1 equivalent).
    let cursor = std::io::Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;

        let stripped: PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }

        let dest_path = dest_dir.join(&stripped);

        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        entry.unpack(&dest_path)?;
    }

    let agent_path = dest_dir.join("cursor-agent");

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

fn cursor_cli_download_url() -> Result<String> {
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

/// Validates format only — real auth is checked on first use.
pub async fn validate_cursor_api_key(api_key: &str) -> Result<()> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        bail!("API key cannot be empty");
    }
    Ok(())
}

// ============================================================================
// Embedding Model (for memory search)
// ============================================================================

pub fn ensure_embedding_model(paths: &Paths) -> Result<()> {
    memory::ensure_model_downloaded(paths)
}

// ============================================================================
// Startup Dependency Check
// ============================================================================

/// Ensure all dependencies for the active backend are installed and up to date.
pub async fn ensure_deps(config: &crate::config::Config, paths: &Paths) -> Result<()> {
    use crate::config::AiBackend;

    match config.backend {
        AiBackend::Claude => {
            ensure_bun(paths).await?;
            ensure_claude_code(paths).await?;
        }
        AiBackend::Cursor => {
            ensure_bun(paths).await?;
            ensure_cursor_cli(paths).await?;
        }
    }

    if config.channels.signal.is_some() {
        ensure_java(paths).await?;
        ensure_signal_cli(paths).await?;
    }

    ensure_embedding_model(paths)?;
    Ok(())
}

/// Run `bun install` for a skill directory if it has a package.json with
/// dependencies but no node_modules. Called at runtime when skills are discovered.
pub fn ensure_skill_deps(bun: &Path, skill_dir: &Path) {
    let pkg_json = skill_dir.join("package.json");
    let node_modules = skill_dir.join("node_modules");

    if !pkg_json.exists() || node_modules.exists() {
        return;
    }

    if let Ok(content) = std::fs::read_to_string(&pkg_json) {
        let has_deps =
            content.contains("\"dependencies\"") && !content.contains("\"dependencies\": {}");
        if !has_deps {
            return;
        }
    } else {
        return;
    }

    let skill_name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    info!("Installing dependencies for skill: {}", skill_name);

    match std::process::Command::new(bun)
        .arg("install")
        .current_dir(skill_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            info!("Dependencies installed for skill: {}", skill_name);
        }
        Ok(output) => {
            warn!(
                "bun install failed for skill {} (exit {:?}): {}",
                skill_name,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            warn!("Failed to run bun install for skill {}: {}", skill_name, e);
        }
    }
}

#[cfg(test)]
mod claude_code_entry_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn linux_host_selects_linux_binary_when_both_platform_packages_are_present() {
        let (_temp, paths) = paths_with(&[]);
        let modules = paths.claude_code_dir.join("node_modules/@anthropic-ai");
        let linux = modules.join("claude-code-linux-x64/claude");
        write_file(&linux, b"\x7fELF\x02\x01\x01\0linux");
        write_file(
            &modules.join("claude-code-darwin-arm64/claude"),
            b"\xcf\xfa\xed\xfe\0darwin",
        );
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Native(linux))
        );
    }

    #[test]
    fn bin_claude_symlink_to_node_shebang_is_classified_as_script() {
        let (_temp, paths) = paths_with(&[]);
        let modules = paths.claude_code_dir.join("node_modules");
        let script = modules.join("@anthropic-ai/claude-code/cli.js");
        let link = modules.join(".bin/claude");
        write_file(&script, b"#!/usr/bin/env node\nconsole.log('hi');");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&script, &link).unwrap();
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Script(link))
        );
    }

    #[test]
    fn foreign_magic_and_non_executable_files_are_refused() {
        let (_temp, paths) = paths_with(&[]);
        let candidate = paths
            .claude_code_dir
            .join("node_modules/@anthropic-ai/claude-code-linux-x64/claude");
        for bytes in [b"random\0data".as_slice(), b"\xcf\xfa\xed\xfe\0darwin"] {
            write_file(&candidate, bytes);
            assert!(find_claude_code_for_host(&paths, "linux", "x86_64").is_none());
        }
        write_file(&candidate, b"\x7fELF\0linux");
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_claude_code_for_host(&paths, "linux", "x86_64").is_none());
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn paths_with(files: &[&str]) -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::for_base(tmp.path().to_path_buf());
        for rel in files {
            let p = paths.claude_code_dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
            write_file(&p, b"#!/bin/sh\n");
        }
        (tmp, paths)
    }

    #[test]
    fn skips_the_text_stub_and_finds_the_platform_binary() {
        let (_t, paths) = paths_with(&[]);
        let modules = paths.claude_code_dir.join("node_modules");
        write_file(
            &modules.join("@anthropic-ai/claude-code/bin/claude.exe"),
            b"echo \"Error: claude native binary not installed.\" >&2\n",
        );
        write_file(
            &modules.join("@anthropic-ai/claude-code-linux-x64/claude"),
            b"\x7fELF\x02\x01\x01\0native",
        );
        write_file(
            &modules.join(".bin/claude"),
            b"echo \"Error: claude native binary not installed.\" >&2\n",
        );

        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Native(
                modules.join("@anthropic-ai/claude-code-linux-x64/claude")
            )),
            "resolved the stub — every turn would fail with Exec format error"
        );
    }

    #[test]
    fn prefers_the_link_when_it_points_at_a_real_binary() {
        let (_t, paths) = paths_with(&[]);
        let modules = paths.claude_code_dir.join("node_modules");
        write_file(&modules.join(".bin/claude"), b"\x7fELF\x02\x01\x01\0linked");
        write_file(
            &modules.join("@anthropic-ai/claude-code-linux-x64/claude"),
            b"\x7fELF\x02\x01\x01\0platform",
        );
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Native(modules.join(".bin/claude")))
        );
    }

    #[test]
    fn falls_back_to_the_script_rather_than_a_stub() {
        let (_t, paths) = paths_with(&[]);
        let modules = paths.claude_code_dir.join("node_modules");
        write_file(
            &modules.join(".bin/claude"),
            b"echo \"Error: claude native binary not installed.\"\n",
        );
        write_file(
            &modules.join("@anthropic-ai/claude-code/cli.js"),
            b"#!/usr/bin/env node\n",
        );
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Script(
                modules.join("@anthropic-ai/claude-code/cli.js")
            ))
        );
    }

    #[test]
    fn resolves_the_linked_native_binary() {
        let (_t, paths) = paths_with(&["node_modules/.bin/claude"]);
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Native(
                paths.claude_code_dir.join("node_modules/.bin/claude")
            ))
        );
    }

    #[test]
    fn falls_back_to_the_legacy_script() {
        let (_t, paths) = paths_with(&["node_modules/@anthropic-ai/claude-code/cli.js"]);
        assert_eq!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Script(
                paths
                    .claude_code_dir
                    .join("node_modules/@anthropic-ai/claude-code/cli.js")
            ))
        );
    }

    #[test]
    fn never_resolves_the_packages_stub() {
        let (_t, paths) = paths_with(&["node_modules/@anthropic-ai/claude-code/bin/claude.exe"]);
        assert_eq!(find_claude_code_for_host(&paths, "linux", "x86_64"), None);
    }

    #[test]
    fn none_when_nothing_is_installed() {
        let (_t, paths) = paths_with(&[]);
        assert_eq!(find_claude_code_for_host(&paths, "linux", "x86_64"), None);
    }

    #[test]
    fn prefers_native_over_a_stale_script() {
        let (_t, paths) = paths_with(&[
            "node_modules/.bin/claude",
            "node_modules/@anthropic-ai/claude-code/cli.js",
        ]);
        assert!(matches!(
            find_claude_code_for_host(&paths, "linux", "x86_64"),
            Some(ClaudeCode::Native(_))
        ));
    }
}
