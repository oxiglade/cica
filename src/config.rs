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
    // Internal paths (hidden from user)
    pub internal_dir: PathBuf,
    pub deps_dir: PathBuf,
    pub bun_dir: PathBuf,
    pub java_dir: PathBuf,
    pub signal_cli_dir: PathBuf,
    pub claude_code_dir: PathBuf,
    pub claude_home: PathBuf,
    pub signal_data_dir: PathBuf,
    // Cursor CLI paths
    pub cursor_cli_dir: PathBuf,
    pub cursor_home: PathBuf,
}

/// Get all Cica paths
pub fn paths() -> Result<Paths> {
    let base = ProjectDirs::from("", "", "cica")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .context("Could not determine config directory")?;

    let internal_dir = base.join("internal");
    let deps_dir = internal_dir.join("deps");

    Ok(Paths {
        config_file: base.join("config.toml"),
        pairing_file: base.join("pairing.json"),
        memory_dir: base.join("memory"),
        skills_dir: base.join("skills"),
        // Internal paths
        internal_dir: internal_dir.clone(),
        deps_dir: deps_dir.clone(),
        bun_dir: deps_dir.join("bun"),
        java_dir: deps_dir.join("java"),
        signal_cli_dir: deps_dir.join("signal-cli"),
        claude_code_dir: deps_dir.join("claude-code"),
        claude_home: internal_dir.join("claude-home"),
        signal_data_dir: internal_dir.join("signal-data"),
        // Cursor CLI paths
        cursor_cli_dir: deps_dir.join("cursor-cli"),
        cursor_home: internal_dir.join("cursor-home"),
        base,
    })
}

impl Paths {
    /// Create all necessary directories and default files
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::create_dir_all(&self.skills_dir)?;
        std::fs::create_dir_all(&self.deps_dir)?;
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

/// Which AI backend to use
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackend {
    #[default]
    Claude,
    Cursor,
}

/// Root configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub channels: ChannelsConfig,

    #[serde(default)]
    pub claude: ClaudeConfig,

    #[serde(default)]
    pub cursor: CursorConfig,

    /// Which AI backend to use (claude or cursor)
    #[serde(default)]
    pub backend: AiBackend,

    /// Global onboarding prompt (can be overridden per channel)
    pub onboarding_prompt: Option<String>,
}

/// All channel configurations
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
    pub signal: Option<SignalConfig>,
    pub slack: Option<SlackConfig>,
}

/// Telegram-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl TelegramConfig {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            ..Default::default()
        }
    }
}

/// Signal-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalConfig {
    #[serde(default)]
    pub phone_number: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl SignalConfig {
    pub fn new(phone_number: String) -> Self {
        Self {
            phone_number,
            ..Default::default()
        }
    }
}

/// Slack-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlackConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub app_token: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl SlackConfig {
    pub fn new(bot_token: String, app_token: String) -> Self {
        Self {
            bot_token,
            app_token,
            ..Default::default()
        }
    }
}

/// Channel settings relevant to pairing/onboarding
#[derive(Debug, Clone, Default)]
pub struct ChannelSettings {
    pub auto_approve: bool,
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl Config {
    pub fn channel_settings(&self, channel: &str) -> ChannelSettings {
        let global_prompt = self.onboarding_prompt.clone();

        match channel {
            "telegram" => self
                .channels
                .telegram
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            "signal" => self
                .channels
                .signal
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            "slack" => self
                .channels
                .slack
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            _ => ChannelSettings::default(),
        }
    }
}

/// Claude configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeConfig {
    /// Anthropic API key or OAuth token (used when not using Vertex AI)
    pub api_key: Option<String>,
    /// Use Google Vertex AI instead of Anthropic API
    #[serde(default)]
    pub use_vertex: bool,
    /// GCP project ID for Vertex AI (required when use_vertex is true)
    pub vertex_project_id: Option<String>,
    /// GCP region for Vertex AI (e.g. "europe-west1", "us-east5"). Defaults to "europe-west1" if unset.
    pub vertex_region: Option<String>,
    /// Path to GCP service account JSON key file (long-lived auth; recommended for servers).
    /// When set, GOOGLE_APPLICATION_CREDENTIALS is set for Claude so gcloud login is not needed.
    pub vertex_credentials_path: Option<String>,
}

/// Cursor CLI configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CursorConfig {
    /// Cursor API key (from dashboard)
    pub api_key: Option<String>,
    /// Model to use (default: claude-sonnet-4-20250514)
    pub model: Option<String>,
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
        if self.channels.slack.is_some() {
            channels.push("slack");
        }

        channels
    }

    /// Check if Claude is configured (Anthropic API key or Vertex AI)
    pub fn is_claude_configured(&self) -> bool {
        if self.claude.use_vertex {
            self.claude
                .vertex_project_id
                .as_ref()
                .is_some_and(|s| !s.is_empty())
        } else {
            self.claude.api_key.is_some()
        }
    }

    /// Check if Cursor is configured
    pub fn is_cursor_configured(&self) -> bool {
        self.cursor.api_key.is_some()
    }

    /// Check if the selected backend is configured
    pub fn is_backend_configured(&self) -> bool {
        match self.backend {
            AiBackend::Claude => self.is_claude_configured(),
            AiBackend::Cursor => self.is_cursor_configured(),
        }
    }
}
