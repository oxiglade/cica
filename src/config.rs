use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ============================================================================
// Paths
// ============================================================================

/// All paths used by Cica
pub struct Paths {
    pub base: PathBuf,
    pub config_file: PathBuf,
    pub pairing_file: PathBuf,
    pub memory_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub claude_code_dir: PathBuf,
    pub claude_home: PathBuf,     // Isolated HOME for Claude Code
    pub java_dir: PathBuf,        // Bundled JRE for signal-cli
    pub signal_cli_dir: PathBuf,  // signal-cli installation
    pub signal_data_dir: PathBuf, // signal-cli account data
}

/// Get all Cica paths
pub fn paths() -> Result<Paths> {
    let base = ProjectDirs::from("", "", "cica")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .context("Could not determine config directory")?;

    Ok(Paths {
        config_file: base.join("config.toml"),
        pairing_file: base.join("pairing.json"),
        memory_dir: base.join("memory"),
        skills_dir: base.join("skills"),
        bin_dir: base.join("bin"),
        claude_code_dir: base.join("claude-code"),
        claude_home: base.join("claude-home"),
        java_dir: base.join("java"),
        signal_cli_dir: base.join("signal-cli"),
        signal_data_dir: base.join("signal-data"),
        base,
    })
}

impl Paths {
    /// Create all necessary directories and default files
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::create_dir_all(&self.skills_dir)?;
        std::fs::create_dir_all(&self.bin_dir)?;
        std::fs::create_dir_all(&self.claude_code_dir)?;
        std::fs::create_dir_all(&self.claude_home)?;

        // Create default PERSONA.md if it doesn't exist
        let persona_path = self.base.join("PERSONA.md");
        if !persona_path.exists() {
            let content = r#"# PERSONA.md - Persona & Boundaries

## Tone & Style
- Keep replies concise and direct.
- Ask clarifying questions when needed.
- Be helpful but honest about limitations.

## Capabilities
You are a personal assistant running on the user's machine. You can:
- Answer questions and have conversations
- Help with writing, brainstorming, and thinking through problems

You do NOT have direct access to:
- Calendars, email, or external services
- The user's files or system (unless given explicit access)
- Real-time information

## Skills
When the user asks for something you can't do directly, suggest creating a **skill** for it.
Skills are custom extensions that live in the skills/ folder. Each skill has:
- A SKILL.md file describing what it does
- Optional scripts to execute actions

Example: "I can't access your calendar directly, but we could create a calendar skill that connects to your calendar service. Want me to help set that up?"
"#;
            std::fs::write(&persona_path, content)?;
        }

        Ok(())
    }
}

// ============================================================================
// Config Types
// ============================================================================

/// Root configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub channels: ChannelsConfig,

    #[serde(default)]
    pub claude: ClaudeConfig,
}

/// All channel configurations
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
    pub signal: Option<SignalConfig>,
}

/// Telegram-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
}

/// Signal-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalConfig {
    /// Phone number with country code (e.g., "+1234567890")
    pub phone_number: String,
}

/// Claude configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeConfig {
    /// Anthropic API key or OAuth token
    pub api_key: Option<String>,
}

// ============================================================================
// Config Operations
// ============================================================================

impl Config {
    /// Load config from the standard location
    pub fn load() -> Result<Self> {
        let path = paths()?.config_file;

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Could not read config file: {:?}", path))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Could not parse config file: {:?}", path))?;

        Ok(config)
    }

    /// Save config to the standard location
    pub fn save(&self) -> Result<()> {
        let paths = paths()?;
        paths.ensure_dirs()?;

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&paths.config_file, content)?;

        Ok(())
    }

    /// Check if config file exists
    pub fn exists() -> Result<bool> {
        Ok(paths()?.config_file.exists())
    }

    /// Get list of configured channel names
    pub fn configured_channels(&self) -> Vec<&'static str> {
        let mut channels = Vec::new();

        if self.channels.telegram.is_some() {
            channels.push("telegram");
        }
        if self.channels.signal.is_some() {
            channels.push("signal");
        }

        channels
    }

    /// Check if Claude is configured
    pub fn is_claude_configured(&self) -> bool {
        self.claude.api_key.is_some()
    }
}
