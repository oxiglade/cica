use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ============================================================================
// Paths
// ============================================================================

/// All paths used by Cica
#[derive(Debug, Clone)]
pub struct Paths {
    pub base: PathBuf,
    pub config_file: PathBuf,
    pub pairing_file: PathBuf,
    pub memory_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub internal_dir: PathBuf,
    pub models_dir: PathBuf,
    pub deps_dir: PathBuf,
    pub bun_dir: PathBuf,
    pub java_dir: PathBuf,
    pub signal_cli_dir: PathBuf,
    pub claude_code_dir: PathBuf,
    pub claude_home: PathBuf,
    pub signal_data_dir: PathBuf,
    pub cursor_cli_dir: PathBuf,
    pub cursor_home: PathBuf,
    pub audit_db: PathBuf,
}

pub fn paths() -> Result<Paths> {
    let base = ProjectDirs::from("", "", "cica")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .context("Could not determine config directory")?;

    Ok(Paths::for_base(base))
}

impl Paths {
    pub fn for_base(base: PathBuf) -> Self {
        let internal_dir = base.join("internal");
        let deps_dir = internal_dir.join("deps");

        Self {
            config_file: base.join("config.toml"),
            pairing_file: base.join("pairing.json"),
            memory_dir: base.join("memory"),
            skills_dir: base.join("skills"),
            internal_dir: internal_dir.clone(),
            models_dir: internal_dir.join("models"),
            deps_dir: deps_dir.clone(),
            bun_dir: deps_dir.join("bun"),
            java_dir: deps_dir.join("java"),
            signal_cli_dir: deps_dir.join("signal-cli"),
            claude_code_dir: deps_dir.join("claude-code"),
            claude_home: internal_dir.join("claude-home"),
            signal_data_dir: internal_dir.join("signal-data"),
            cursor_cli_dir: deps_dir.join("cursor-cli"),
            cursor_home: internal_dir.join("cursor-home"),
            audit_db: base.join("audit.db"),
            base,
        }
    }

    /// Builds isolated mutable paths for a worker while retaining router-owned inputs.
    pub fn for_worker(base: PathBuf, router: &Paths) -> Self {
        let internal_dir = base.join("internal");
        Self {
            base: base.clone(),
            config_file: router.config_file.clone(),
            pairing_file: base.join("pairing.json"),
            memory_dir: base.join("memory"),
            skills_dir: router.skills_dir.clone(),
            internal_dir: internal_dir.clone(),
            models_dir: router.models_dir.clone(),
            deps_dir: router.deps_dir.clone(),
            bun_dir: router.bun_dir.clone(),
            java_dir: router.java_dir.clone(),
            signal_cli_dir: router.signal_cli_dir.clone(),
            claude_code_dir: router.claude_code_dir.clone(),
            claude_home: internal_dir.join("claude-home"),
            signal_data_dir: internal_dir.join("signal-data"),
            cursor_cli_dir: router.cursor_cli_dir.clone(),
            cursor_home: internal_dir.join("cursor-home"),
            audit_db: base.join("audit.db"),
        }
    }

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

#[cfg(test)]
pub(crate) fn test_paths() -> (tempfile::TempDir, Paths) {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = Paths::for_base(temp.path().to_path_buf());
    (temp, paths)
}

fn default_true() -> bool {
    true
}

// ============================================================================
// Config Types
// ============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackend {
    #[default]
    Claude,
    Cursor,
}

/// Which durable state store to use (none = all-local, today's behavior).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Filesystem,
    S3,
}

/// Where a turn executes (none/local = in-process; subprocess = one-shot worker).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Local,
    Subprocess,
    Docker,
    Fargate,
}

/// Whether to `bun install` skill deps on this host at discovery time. Only
/// `false` for Fargate, where turns run on a remote worker that hydrates its own
/// skills copy and installs deps on demand — so installing here is wasted work.
pub fn prep_skill_deps_locally(provider: Option<ProviderKind>) -> bool {
    !matches!(provider, Some(ProviderKind::Fargate))
}

/// S3 state-store settings (used when `store = "s3"`). Credentials come from the
/// standard AWS provider chain (env / instance role), never config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    /// Bucket name (required).
    pub bucket: String,
    /// AWS region; falls back to the default chain when unset.
    #[serde(default)]
    pub region: Option<String>,
    /// Optional key namespace within the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Optional endpoint override (LocalStack / MinIO / testing).
    #[serde(default)]
    pub endpoint: Option<String>,
}

fn default_container_name() -> String {
    "cica-worker".to_string()
}
fn default_poll_interval_secs() -> u64 {
    5
}
fn default_worker_idle_secs() -> u64 {
    600
}
fn default_worker_start_timeout_secs() -> u64 {
    180
}
fn default_turn_timeout_secs() -> u64 {
    900
}
fn default_worker_cap() -> usize {
    32
}
fn default_worker_max_age_secs() -> u64 {
    86_400
}

/// Fargate launcher settings (used when `provider = "fargate"`). Credentials
/// come from the task IAM role (the AWS chain), never config.
///
/// Field defaults (`container_name`, `poll_interval_secs`) are
/// supplied by serde on parse — `Default::default()` leaves them empty/zero, so
/// always deserialize this from TOML rather than constructing it directly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FargateConfig {
    /// ECS cluster name or ARN (required).
    pub cluster: String,
    /// Task-definition family or `family:revision` (required).
    pub task_definition: String,
    /// awsvpc subnets to launch into (required in practice).
    #[serde(default)]
    pub subnets: Vec<String>,
    /// Security groups; default none.
    #[serde(default)]
    pub security_groups: Vec<String>,
    /// Assign a public IP (default false — private subnets + NAT).
    #[serde(default)]
    pub assign_public_ip: bool,
    /// AWS region; falls back to the default chain when unset.
    #[serde(default)]
    pub region: Option<String>,
    /// Which container in the task-def to override with `worker --turn <id>`.
    #[serde(default = "default_container_name")]
    pub container_name: String,
    /// DescribeTasks poll interval in seconds.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

/// Distributed-deployment configuration. All optional; absent = single-box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentConfig {
    /// State store backend. `None` disables hydration (default).
    #[serde(default)]
    pub store: Option<StoreKind>,
    /// Filesystem store root. Defaults to `internal/state-store` when unset.
    #[serde(default)]
    pub state_path: Option<String>,
    /// Turn execution mode. `None` (or `Local`) = in-process (default).
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    /// Worker image for `provider = "docker"` (default `cica-worker:latest`).
    #[serde(default)]
    pub docker_image: Option<String>,
    /// S3 store settings (used when `store = "s3"`).
    #[serde(default)]
    pub s3: Option<S3Config>,
    /// Fargate launcher settings (used when `provider = "fargate"`).
    #[serde(default)]
    pub fargate: Option<FargateConfig>,
    #[serde(default = "default_worker_idle_secs")]
    pub worker_idle_secs: u64,
    #[serde(default = "default_worker_start_timeout_secs")]
    pub worker_start_timeout_secs: u64,
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
    #[serde(default = "default_worker_cap")]
    pub worker_cap: usize,
    #[serde(default = "default_worker_max_age_secs")]
    pub worker_max_age_secs: u64,
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            store: None,
            state_path: None,
            provider: None,
            docker_image: None,
            s3: None,
            fargate: None,
            worker_idle_secs: default_worker_idle_secs(),
            worker_start_timeout_secs: default_worker_start_timeout_secs(),
            turn_timeout_secs: default_turn_timeout_secs(),
            worker_cap: default_worker_cap(),
            worker_max_age_secs: default_worker_max_age_secs(),
        }
    }
}

impl DeploymentConfig {
    pub fn policy_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = format!(
            "worker_idle_secs={};worker_start_timeout_secs={};turn_timeout_secs={};worker_cap={};worker_max_age_secs={}",
            self.worker_idle_secs,
            self.worker_start_timeout_secs,
            self.turn_timeout_secs,
            self.worker_cap,
            self.worker_max_age_secs
        );
        format!("{:x}", Sha256::digest(canonical.as_bytes()))
    }
}

fn default_skills_ref() -> String {
    "main".to_string()
}

fn default_skills_refresh_secs() -> u64 {
    600
}

/// Skills git-sync settings (router-side). When present, the router periodically
/// pulls `repo` at `ref` into the skills directory and mirrors it to the state
/// store under "skills" for workers to hydrate. The git credential is read from
/// the `CICA_SKILLS_GIT_TOKEN` env var, never from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Git repository URL (required), e.g. https://github.com/your-org/ai-skills.
    pub repo: String,
    /// Branch, tag, or sha to check out.
    #[serde(default = "default_skills_ref", rename = "ref")]
    pub git_ref: String,
    /// Seconds between re-pulls.
    #[serde(default = "default_skills_refresh_secs")]
    pub refresh_secs: u64,
}

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

    /// Distributed-deployment settings (state store, etc.)
    #[serde(default)]
    pub deployment: DeploymentConfig,

    /// Skills git-sync settings (router-side). Absent = no skills sync.
    #[serde(default)]
    pub skills: Option<SkillsConfig>,

    /// Enable audit logging of conversations and system events (default: true)
    #[serde(default = "default_true")]
    pub audit: bool,

    /// Global onboarding prompt (can be overridden per channel)
    pub onboarding_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
    pub signal: Option<SignalConfig>,
    pub slack: Option<SlackConfig>,
    pub linear: Option<LinearConfig>,
}

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
    /// Allow Slack to unfurl (preview) links in bot messages (default: false)
    #[serde(default)]
    pub unfurl_links: bool,
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

fn default_linear_listen() -> String {
    "0.0.0.0:8080".to_string()
}

/// Linear agent channel. Unlike every other channel this one is *inbound*: Linear
/// POSTs an `AgentSessionEvent` webhook when the app is @mentioned on an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearConfig {
    /// The OAuth application's client credentials. Preferred: they mint 30-day
    /// **app-actor** tokens on demand, so activities are authored by the app
    /// user and nothing expires under a running channel.
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    /// Accepted and ignored.
    ///
    /// A Linear app has a single grant: minting a client-credentials token
    /// revokes any outstanding authorization-code grant, so a channel holding a
    /// refresh token loses it as soon as anything else authenticates as the
    /// same app. Kept only so an existing config does not fail to parse; the
    /// channel warns if it is set.
    #[serde(default)]
    pub refresh_token: String,
    /// A pre-minted access token. Accepted for local testing only — Linear's
    /// authorization-code tokens last 24 hours, so a token pasted here stops
    /// working after a day. Ignored when a refresh token or client credentials
    /// are set.
    #[serde(default)]
    pub access_token: String,
    /// Webhook signing secret, from the webhook's detail page. Used to verify the
    /// `Linear-Signature` header over the raw request body.
    #[serde(default)]
    pub webhook_secret: String,
    /// Where the webhook listener binds. Behind a TLS terminator (ALB, reverse
    /// proxy) — cica never terminates TLS itself.
    #[serde(default = "default_linear_listen")]
    pub listen_addr: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
    /// Maps a Linear user's email to an identity on another channel, so the same
    /// human keeps one set of memories and one USER.md. Values are
    /// `"<channel>:<user_id>"`, e.g. `"slack:U0123ABC"`.
    #[serde(default)]
    pub identity: HashMap<String, String>,
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            refresh_token: String::new(),
            access_token: String::new(),
            webhook_secret: String::new(),
            listen_addr: default_linear_listen(),
            auto_approve: false,
            shared_identity: false,
            onboarding_prompt: None,
            identity: HashMap::new(),
        }
    }
}

impl LinearConfig {
    pub fn new(client_id: String, client_secret: String, webhook_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            webhook_secret,
            ..Default::default()
        }
    }

    /// True when the channel has some way to authenticate.
    pub fn has_credential(&self) -> bool {
        (!self.client_id.is_empty() && !self.client_secret.is_empty())
            || !self.access_token.is_empty()
    }

    /// True when the client credentials can be used to refresh an installed
    /// agent's token, rather than to mint a fresh client-credentials one.
    pub fn can_refresh(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    /// Resolve a Linear commenter to the identity their memories are keyed under.
    /// Falls back to `("linear", linear_user_id)` when no mapping applies, which
    /// takes the person through the normal pairing flow.
    pub fn resolve_identity(&self, email: Option<&str>, linear_user_id: &str) -> (String, String) {
        if let Some(email) = email
            && let Some(mapped) = self.identity.get(&email.to_lowercase())
            && let Some((channel, user_id)) = mapped.split_once(':')
            && !channel.is_empty()
            && !user_id.is_empty()
        {
            return (channel.to_string(), user_id.to_string());
        }
        ("linear".to_string(), linear_user_id.to_string())
    }
}

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
            "linear" => self
                .channels
                .linear
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeConfig {
    /// Anthropic API key or OAuth token (used when not using Vertex AI)
    pub api_key: Option<String>,
    /// Model to use: an alias ("sonnet", "opus") or full model ID from the API (e.g. "claude-sonnet-4-5-20250929")
    pub model: Option<String>,
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
    /// Use Amazon Bedrock instead of the Anthropic API. Takes precedence over `use_vertex`.
    #[serde(default)]
    pub use_bedrock: bool,
    /// AWS region for Bedrock (e.g. "eu-central-1"). When unset, Claude Code resolves
    /// the region itself from the ambient AWS environment.
    pub bedrock_region: Option<String>,
}

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

/// Truthy/falsy values accepted for boolean env overlays, case-insensitive.
/// An unrecognised value is ignored with a warning so a typo leaves the config
/// file's value in place instead of silently flipping it.
fn env_bool(name: &str, value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            tracing::warn!("ignoring invalid {name}={other}: expected a boolean");
            None
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = paths()?.config_file;
        Self::load_from(&path)
    }

    /// Loads configuration from an explicit file and then applies the environment overlay.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let mut config: Config = match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content)
                .with_context(|| format!("Could not parse config file: {path:?}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("no config.toml at {path:?}; using defaults + environment");
                Config::default()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Could not read config file: {path:?}"));
            }
        };
        config.apply_env_overlay();
        Ok(config)
    }

    /// Overlay deployment-relevant config and credential secrets from the
    /// process environment. Lets a cloud worker run with NO `config.toml` —
    /// everything (backend, store, S3 coords, AI keys, model) comes from the
    /// task env. A provider that cannot deliver the operator's `config.toml`
    /// to the worker (Fargate: command override only, no bind mount) can only
    /// configure what is listed here.
    pub(crate) fn apply_env_overlay(&mut self) {
        self.overlay_from_env(|k| std::env::var(k).ok());
    }

    /// Env overlay core, parameterized by a lookup so it is testable without
    /// touching the global process environment.
    fn overlay_from_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("CICA_CURSOR_API_KEY") {
            self.cursor.api_key = Some(v);
        }
        if let Some(v) = get("CICA_CLAUDE_API_KEY") {
            self.claude.api_key = Some(v);
        }
        if let Some(v) = get("CICA_CURSOR_MODEL") {
            self.cursor.model = Some(v);
        }
        if let Some(v) = get("CICA_CLAUDE_MODEL") {
            self.claude.model = Some(v);
        }
        if let Some(v) =
            get("CICA_CLAUDE_USE_BEDROCK").and_then(|v| env_bool("CICA_CLAUDE_USE_BEDROCK", &v))
        {
            self.claude.use_bedrock = v;
        }
        if let Some(v) = get("CICA_CLAUDE_BEDROCK_REGION") {
            self.claude.bedrock_region = Some(v);
        }
        if let Some(v) = get("CICA_BACKEND") {
            match v.as_str() {
                "cursor" => self.backend = AiBackend::Cursor,
                "claude" => self.backend = AiBackend::Claude,
                other => tracing::warn!("ignoring unknown CICA_BACKEND={other}"),
            }
        }
        if let Some(v) = get("CICA_STORE") {
            match v.as_str() {
                "s3" => self.deployment.store = Some(StoreKind::S3),
                "filesystem" => self.deployment.store = Some(StoreKind::Filesystem),
                other => tracing::warn!("ignoring unknown CICA_STORE={other}"),
            }
        }
        if let Some(v) = get("CICA_STATE_PATH") {
            self.deployment.state_path = Some(v);
        }
        macro_rules! overlay_number {
            ($name:literal, $field:ident) => {
                if let Some(value) = get($name) {
                    match value.parse() {
                        Ok(value) => self.deployment.$field = value,
                        Err(error) => tracing::warn!("ignoring invalid {}={value}: {error}", $name),
                    }
                }
            };
        }
        overlay_number!("CICA_WORKER_IDLE_SECS", worker_idle_secs);
        overlay_number!("CICA_WORKER_START_TIMEOUT_SECS", worker_start_timeout_secs);
        overlay_number!("CICA_TURN_TIMEOUT_SECS", turn_timeout_secs);
        overlay_number!("CICA_WORKER_CAP", worker_cap);
        overlay_number!("CICA_WORKER_MAX_AGE_SECS", worker_max_age_secs);
        if let Some(v) = get("CICA_LINEAR_CLIENT_ID") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .client_id = v;
        }
        if let Some(v) = get("CICA_LINEAR_CLIENT_SECRET") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .client_secret = v;
        }
        if let Some(v) = get("CICA_LINEAR_REFRESH_TOKEN") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .refresh_token = v;
        }
        if let Some(v) = get("CICA_LINEAR_ACCESS_TOKEN") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .access_token = v;
        }
        if let Some(v) = get("CICA_LINEAR_WEBHOOK_SECRET") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .webhook_secret = v;
        }
        if let Some(v) = get("CICA_LINEAR_LISTEN_ADDR") {
            self.channels
                .linear
                .get_or_insert_with(Default::default)
                .listen_addr = v;
        }
        if let Some(v) = get("CICA_S3_BUCKET") {
            self.deployment
                .s3
                .get_or_insert_with(Default::default)
                .bucket = v;
        }
        if let Some(v) = get("CICA_S3_REGION") {
            self.deployment
                .s3
                .get_or_insert_with(Default::default)
                .region = Some(v);
        }
    }

    pub fn save(&self) -> Result<()> {
        let paths = paths()?;
        paths.ensure_dirs()?;

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&paths.config_file, content)?;

        Ok(())
    }

    pub fn exists() -> Result<bool> {
        Ok(paths()?.config_file.exists())
    }

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
        if self.channels.linear.is_some() {
            channels.push("linear");
        }

        channels
    }

    pub fn is_claude_configured(&self) -> bool {
        if self.claude.use_bedrock {
            // Nothing to check: AWS credentials come from the provider chain
            // (task role, instance profile, shared profile) and the region can
            // come from the ambient AWS environment, so neither is visible here.
            true
        } else if self.claude.use_vertex {
            self.claude
                .vertex_project_id
                .as_ref()
                .is_some_and(|s| !s.is_empty())
        } else {
            self.claude.api_key.is_some()
        }
    }

    pub fn is_cursor_configured(&self) -> bool {
        self.cursor.api_key.is_some()
    }

    pub fn is_backend_configured(&self) -> bool {
        match self.backend {
            AiBackend::Claude => self.is_claude_configured(),
            AiBackend::Cursor => self.is_cursor_configured(),
        }
    }

    pub fn model_for(&self, backend: AiBackend) -> Option<String> {
        match backend {
            AiBackend::Claude => self.claude.model.clone(),
            AiBackend::Cursor => self.cursor.model.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_with_mapping() -> LinearConfig {
        let mut config = LinearConfig::new("id".into(), "secret".into(), "wh".into());
        config
            .identity
            .insert("rodrigo@rootglobal.io".into(), "slack:U0123ABC".into());
        config
    }

    #[test]
    fn a_mapped_email_resolves_to_the_other_channels_identity() {
        let config = linear_with_mapping();
        assert_eq!(
            config.resolve_identity(Some("rodrigo@rootglobal.io"), "usr_1"),
            ("slack".to_string(), "U0123ABC".to_string())
        );
    }

    #[test]
    fn email_matching_ignores_case() {
        // Linear hands back whatever casing the account was created with.
        let config = linear_with_mapping();
        assert_eq!(
            config.resolve_identity(Some("Rodrigo@RootGlobal.io"), "usr_1"),
            ("slack".to_string(), "U0123ABC".to_string())
        );
    }

    #[test]
    fn an_unmapped_person_stays_a_linear_user_and_goes_through_pairing() {
        let config = linear_with_mapping();
        assert_eq!(
            config.resolve_identity(Some("someone@else.com"), "usr_2"),
            ("linear".to_string(), "usr_2".to_string())
        );
        assert_eq!(
            config.resolve_identity(None, "usr_3"),
            ("linear".to_string(), "usr_3".to_string())
        );
    }

    #[test]
    fn a_malformed_mapping_falls_back_rather_than_producing_half_an_identity() {
        let mut config = LinearConfig::new("id".into(), "secret".into(), "wh".into());
        config.identity.insert("a@b.c".into(), "slack".into());
        config.identity.insert("d@e.f".into(), "slack:".into());
        config.identity.insert("g@h.i".into(), ":U1".into());

        for email in ["a@b.c", "d@e.f", "g@h.i"] {
            assert_eq!(
                config.resolve_identity(Some(email), "usr_9"),
                ("linear".to_string(), "usr_9".to_string()),
                "{email} should not produce a partial identity"
            );
        }
    }

    #[test]
    fn a_credential_is_either_the_client_pair_or_a_static_token() {
        assert!(!LinearConfig::default().has_credential());

        let pair = LinearConfig::new("id".into(), "secret".into(), "wh".into());
        assert!(pair.has_credential());

        // Half a pair is not a credential.
        let half = LinearConfig {
            client_id: "id".into(),
            ..Default::default()
        };
        assert!(!half.has_credential());

        let static_only = LinearConfig {
            access_token: "lin_static".into(),
            ..Default::default()
        };
        assert!(static_only.has_credential());
    }

    #[test]
    fn a_linear_config_from_env_alone_still_has_a_listen_address() {
        // The env overlay reaches for Default::default(), which must not leave
        // the listener bound to an empty string.
        assert_eq!(LinearConfig::default().listen_addr, "0.0.0.0:8080");
    }

    #[test]
    fn linear_joins_the_configured_channels() {
        let mut config = Config::default();
        assert!(!config.configured_channels().contains(&"linear"));

        config.channels.linear = Some(LinearConfig::default());
        assert!(config.configured_channels().contains(&"linear"));

        // Without a channel_settings arm this silently defaults, which would
        // disable onboarding for every Linear user.
        config.channels.linear = Some(LinearConfig {
            auto_approve: true,
            ..Default::default()
        });
        assert!(config.channel_settings("linear").auto_approve);
    }

    #[test]
    fn worker_paths_isolate_mutable_state_and_share_inputs() {
        let router = Paths::for_base(PathBuf::from("/router"));
        let worker = Paths::for_worker(PathBuf::from("/worker"), &router);

        assert_eq!(worker.base, PathBuf::from("/worker"));
        assert_eq!(worker.config_file, router.config_file);
        assert_eq!(worker.pairing_file, PathBuf::from("/worker/pairing.json"));
        assert_eq!(worker.memory_dir, PathBuf::from("/worker/memory"));
        assert_eq!(worker.skills_dir, router.skills_dir);
        assert_eq!(worker.internal_dir, PathBuf::from("/worker/internal"));
        assert_eq!(worker.models_dir, router.models_dir);
        assert_eq!(worker.deps_dir, router.deps_dir);
        assert_eq!(worker.bun_dir, router.bun_dir);
        assert_eq!(worker.java_dir, router.java_dir);
        assert_eq!(worker.signal_cli_dir, router.signal_cli_dir);
        assert_eq!(worker.claude_code_dir, router.claude_code_dir);
        assert_eq!(
            worker.claude_home,
            PathBuf::from("/worker/internal/claude-home")
        );
        assert_eq!(
            worker.signal_data_dir,
            PathBuf::from("/worker/internal/signal-data")
        );
        assert_eq!(worker.cursor_cli_dir, router.cursor_cli_dir);
        assert_eq!(
            worker.cursor_home,
            PathBuf::from("/worker/internal/cursor-home")
        );
        assert_eq!(worker.audit_db, PathBuf::from("/worker/audit.db"));
    }

    #[test]
    fn deployment_defaults_to_no_store() {
        let cfg = Config::default();
        assert!(cfg.deployment.store.is_none());
    }

    #[test]
    fn deployment_parses_filesystem_store() {
        let toml = r#"
            backend = "claude"
            [deployment]
            store = "filesystem"
            state_path = "/tmp/cica-state"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::Filesystem));
        assert_eq!(
            cfg.deployment.state_path.as_deref(),
            Some("/tmp/cica-state")
        );
    }

    #[test]
    fn provider_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.deployment.provider.is_none());
    }

    #[test]
    fn provider_parses_subprocess() {
        let toml = r#"
            [deployment]
            provider = "subprocess"
            store = "filesystem"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Subprocess));
    }

    #[test]
    fn provider_parses_docker_with_image() {
        let toml = r#"
            [deployment]
            provider = "docker"
            store = "filesystem"
            docker_image = "cica-worker:dev"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Docker));
        assert_eq!(
            cfg.deployment.docker_image.as_deref(),
            Some("cica-worker:dev")
        );
    }

    #[test]
    fn store_parses_s3() {
        let toml = r#"
            [deployment]
            store = "s3"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
    }

    #[test]
    fn deployment_s3_section_parses() {
        let toml = r#"
            [deployment]
            [deployment.s3]
            bucket = "cica-state"
            region = "eu-west-1"
            prefix = "cica"
            endpoint = "http://localhost:4566"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let s3 = cfg.deployment.s3.unwrap();
        assert_eq!(s3.bucket, "cica-state");
        assert_eq!(s3.region.as_deref(), Some("eu-west-1"));
        assert_eq!(s3.prefix.as_deref(), Some("cica"));
        assert_eq!(s3.endpoint.as_deref(), Some("http://localhost:4566"));
    }

    #[test]
    fn env_overlay_sets_cursor_and_claude_keys() {
        let mut cfg = Config::default();
        assert!(cfg.cursor.api_key.is_none());
        let env = |k: &str| match k {
            "CICA_CURSOR_API_KEY" => Some("cur-secret".to_string()),
            "CICA_CLAUDE_API_KEY" => Some("claude-secret".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("cur-secret"));
        assert_eq!(cfg.claude.api_key.as_deref(), Some("claude-secret"));
    }

    #[test]
    fn env_overlay_sets_claude_and_cursor_models() {
        let mut cfg = Config::default();
        assert!(cfg.claude.model.is_none());
        let env = |k: &str| match k {
            "CICA_CLAUDE_MODEL" => Some("opus".to_string()),
            "CICA_CURSOR_MODEL" => Some("auto".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.claude.model.as_deref(), Some("opus"));
        assert_eq!(cfg.cursor.model.as_deref(), Some("auto"));
    }

    #[test]
    fn env_overlay_enables_bedrock_with_region() {
        let mut cfg = Config::default();
        assert!(!cfg.claude.use_bedrock);
        let env = |k: &str| match k {
            "CICA_CLAUDE_USE_BEDROCK" => Some("1".to_string()),
            "CICA_CLAUDE_BEDROCK_REGION" => Some("eu-central-1".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert!(cfg.claude.use_bedrock);
        assert_eq!(cfg.claude.bedrock_region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn env_overlay_ignores_unparseable_bedrock_flag() {
        let mut cfg = Config::default();
        cfg.claude.use_bedrock = true;
        cfg.overlay_from_env(|k| (k == "CICA_CLAUDE_USE_BEDROCK").then(|| "maybe".to_string()));
        // A typo must not silently send turns back to the Anthropic API.
        assert!(cfg.claude.use_bedrock);
    }

    #[test]
    fn env_overlay_can_disable_bedrock() {
        let mut cfg = Config::default();
        cfg.claude.use_bedrock = true;
        cfg.overlay_from_env(|k| (k == "CICA_CLAUDE_USE_BEDROCK").then(|| "false".to_string()));
        assert!(!cfg.claude.use_bedrock);
    }

    #[test]
    fn bedrock_needs_no_anthropic_credential() {
        let mut cfg = Config {
            backend: AiBackend::Claude,
            ..Default::default()
        };
        assert!(!cfg.is_claude_configured());
        cfg.claude.use_bedrock = true;
        assert!(cfg.is_claude_configured());
        assert!(cfg.is_backend_configured());
    }

    #[test]
    fn env_overlay_model_overrides_config_file_value() {
        let mut cfg = Config::default();
        cfg.claude.model = Some("from-file".into());
        cfg.overlay_from_env(|k| (k == "CICA_CLAUDE_MODEL").then(|| "opus".to_string()));
        assert_eq!(cfg.claude.model.as_deref(), Some("opus"));
    }

    #[test]
    fn env_overlay_leaves_model_when_env_absent() {
        let mut cfg = Config::default();
        cfg.claude.model = Some("from-file".into());
        cfg.overlay_from_env(|_| None);
        assert_eq!(cfg.claude.model.as_deref(), Some("from-file"));
    }

    #[test]
    fn env_overlay_leaves_config_value_when_env_absent() {
        let mut cfg = Config::default();
        cfg.cursor.api_key = Some("from-file".into());
        cfg.overlay_from_env(|_| None);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("from-file"));
    }

    #[test]
    fn env_overlay_sets_backend_store_and_s3() {
        let mut cfg = Config::default();
        let env = |k: &str| match k {
            "CICA_BACKEND" => Some("cursor".to_string()),
            "CICA_STORE" => Some("s3".to_string()),
            "CICA_S3_BUCKET" => Some("cica-state".to_string()),
            "CICA_S3_REGION" => Some("eu-central-1".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.backend, AiBackend::Cursor);
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
        let s3 = cfg.deployment.s3.unwrap();
        assert_eq!(s3.bucket, "cica-state");
        assert_eq!(s3.region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn env_overlay_sets_state_path() {
        let mut cfg = Config::default();
        cfg.overlay_from_env(|key| {
            (key == "CICA_STATE_PATH").then(|| "/data/cica/internal/state-store".to_string())
        });
        assert_eq!(
            cfg.deployment.state_path.as_deref(),
            Some("/data/cica/internal/state-store")
        );
    }

    #[test]
    fn env_overlay_ignores_unknown_backend() {
        let mut cfg = Config::default();
        let before = cfg.backend;
        cfg.overlay_from_env(|k| (k == "CICA_BACKEND").then(|| "bogus".to_string()));
        assert_eq!(cfg.backend, before);
    }

    #[test]
    fn worker_config_assembles_from_defaults_plus_env() {
        let mut cfg = Config::default();
        let env = |k: &str| match k {
            "CICA_BACKEND" => Some("cursor".to_string()),
            "CICA_STORE" => Some("s3".to_string()),
            "CICA_S3_BUCKET" => Some("b".to_string()),
            "CICA_S3_REGION" => Some("r".to_string()),
            "CICA_CURSOR_API_KEY" => Some("sekret".to_string()),
            "CICA_CURSOR_MODEL" => Some("auto".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.backend, AiBackend::Cursor);
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("sekret"));
        assert_eq!(cfg.cursor.model.as_deref(), Some("auto"));
    }

    #[test]
    fn provider_parses_fargate() {
        let toml = r#"
            [deployment]
            provider = "fargate"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Fargate));
    }

    #[test]
    fn deployment_fargate_section_parses_with_defaults() {
        let toml = r#"
            [deployment]
            [deployment.fargate]
            cluster = "cica"
            task_definition = "cica-worker"
            subnets = ["subnet-a", "subnet-b"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let f = cfg.deployment.fargate.unwrap();
        assert_eq!(f.cluster, "cica");
        assert_eq!(f.task_definition, "cica-worker");
        assert_eq!(f.subnets, vec!["subnet-a", "subnet-b"]);
        assert!(f.security_groups.is_empty());
        assert!(!f.assign_public_ip);
        assert_eq!(f.region, None);
        assert_eq!(f.container_name, "cica-worker");
        assert_eq!(f.poll_interval_secs, 5);
    }

    #[test]
    fn parses_skills_section() {
        let toml = r#"
backend = "claude"
[skills]
repo = "https://github.com/your-org/ai-skills"
ref = "v2.0"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let s = cfg.skills.expect("skills present");
        assert_eq!(s.repo, "https://github.com/your-org/ai-skills");
        assert_eq!(s.git_ref, "v2.0");
        assert_eq!(s.refresh_secs, 600);
    }

    #[test]
    fn skills_absent_is_none() {
        let cfg: Config = toml::from_str(r#"backend = "claude""#).unwrap();
        assert!(cfg.skills.is_none());
    }

    #[test]
    fn skills_defaults_applied() {
        let cfg: Config = toml::from_str("[skills]\nrepo = \"x\"\n").unwrap();
        let s = cfg.skills.unwrap();
        assert_eq!(s.git_ref, "main");
        assert_eq!(s.refresh_secs, 600);
    }

    #[test]
    fn skill_deps_prepped_locally_except_fargate() {
        // Single-box / local execution → prep deps on this host.
        assert!(prep_skill_deps_locally(None));
        assert!(prep_skill_deps_locally(Some(ProviderKind::Local)));
        assert!(prep_skill_deps_locally(Some(ProviderKind::Subprocess)));
        assert!(prep_skill_deps_locally(Some(ProviderKind::Docker)));
        // Remote worker hydrates its own skills + installs on demand → skip.
        assert!(!prep_skill_deps_locally(Some(ProviderKind::Fargate)));
    }
}
