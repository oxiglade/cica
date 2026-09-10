//! Sandbox abstraction: where an agent turn executes.
//!
//! Phase 1 provides only `LocalProcessProvider`, which runs the agent as a
//! local subprocess (today's behavior). Later phases add container-based
//! providers behind the same `SandboxProvider` trait.

pub mod artifacts;
#[cfg(feature = "fargate")]
mod fargate;
pub mod hydrating;
mod local;
pub mod state;
pub mod warm;
pub mod worker;

pub use local::{LocalProcessProvider, query_result_from_turn};

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::config::{AiBackend, Config, Paths};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ClaudeTarget {
    Anthropic,
    Bedrock { region: Option<String> },
    Vertex { project: String, region: String },
}

impl ClaudeTarget {
    pub fn from_config(config: &crate::config::ClaudeConfig) -> Self {
        if config.use_bedrock {
            Self::Bedrock {
                region: config.bedrock_region.clone().filter(|s| !s.is_empty()),
            }
        } else if config.use_vertex {
            Self::Vertex {
                project: config.vertex_project_id.clone().unwrap_or_default(),
                region: config
                    .vertex_region
                    .clone()
                    .unwrap_or_else(|| "europe-west1".into()),
            }
        } else {
            Self::Anthropic
        }
    }

    pub fn validate_distributed(&self) -> Result<()> {
        let valid_region = |region: &str| {
            !region.is_empty()
                && region
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        };
        match self {
            Self::Bedrock { region } => anyhow::ensure!(
                region.as_deref().is_some_and(valid_region),
                "distributed Bedrock turns require a valid explicit bedrock_region"
            ),
            Self::Vertex { project, region } => anyhow::ensure!(
                !project.trim().is_empty() && valid_region(region),
                "distributed Vertex turns require an explicit project and a valid region"
            ),
            Self::Anthropic => {}
        }
        Ok(())
    }
}

/// A single agent turn to execute.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Affinity {
    Chat {
        channel: String,
        user: String,
    },
    SlackThread {
        channel_id: String,
        thread_ts: String,
    },
    Cron {
        job_id: String,
    },
}

impl Affinity {
    pub fn id(&self) -> String {
        let (tag, fields): (u8, Vec<&str>) = match self {
            Self::Chat { channel, user } => (0, vec![channel, user]),
            Self::SlackThread {
                channel_id,
                thread_ts,
            } => (1, vec![channel_id, thread_ts]),
            Self::Cron { job_id } => (2, vec![job_id]),
        };
        let mut bytes = vec![tag];
        for field in fields {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field.as_bytes());
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionPersistence {
    #[default]
    Resume,
    None,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnJob {
    pub channel: String,
    pub user_id: String,
    pub affinity: Affinity,
    #[serde(default)]
    pub session_persistence: SessionPersistence,
    /// The user/cron prompt to send to the agent.
    pub prompt: String,
    /// System prompt (full on new session, appended on resume — backend decides).
    pub system_prompt: Option<String>,
    /// Backend session id to resume, if any.
    pub resume_session: Option<String>,
    pub skip_permissions: bool,
    pub backend: AiBackend,
    /// Model alias or full id selected by the router.
    pub model: Option<String>,
    /// `None` is a legacy job: use the worker's provider configuration.
    pub claude_target: Option<ClaudeTarget>,
    /// Workspace-relative paths of attachments this turn references.
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(serde::Deserialize)]
struct TurnJobWire {
    channel: String,
    user_id: String,
    #[serde(default)]
    affinity: Option<Affinity>,
    #[serde(default)]
    session_persistence: SessionPersistence,
    prompt: String,
    system_prompt: Option<String>,
    resume_session: Option<String>,
    skip_permissions: bool,
    backend: AiBackend,
    model: Option<String>,
    claude_target: Option<ClaudeTarget>,
    #[serde(default)]
    attachments: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for TurnJob {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = TurnJobWire::deserialize(deserializer)?;
        let affinity = wire.affinity.unwrap_or_else(|| Affinity::Chat {
            channel: wire.channel.clone(),
            user: wire.user_id.clone(),
        });
        Ok(Self {
            channel: wire.channel,
            user_id: wire.user_id,
            affinity,
            session_persistence: wire.session_persistence,
            prompt: wire.prompt,
            system_prompt: wire.system_prompt,
            resume_session: wire.resume_session,
            skip_permissions: wire.skip_permissions,
            backend: wire.backend,
            model: wire.model,
            claude_target: wire.claude_target,
            attachments: wire.attachments,
        })
    }
}

impl TurnJob {
    /// The router's turn contract: backend, target, and model come from the router's
    /// config, and the worker honours them regardless of its own environment.
    pub fn new(
        config: &Config,
        channel: &str,
        user_id: &str,
        affinity: Affinity,
        prompt: String,
        system_prompt: Option<String>,
        resume_session: Option<String>,
    ) -> Self {
        Self {
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            affinity,
            session_persistence: SessionPersistence::Resume,
            prompt,
            system_prompt,
            resume_session,
            skip_permissions: true,
            backend: config.backend,
            model: config.model_for(config.backend),
            claude_target: Some(ClaudeTarget::from_config(&config.claude)),
            attachments: Vec::new(),
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<String>) -> Self {
        self.attachments = attachments;
        self
    }
}

pub const PROTOCOL_VERSION: u32 = 1;

/// Wire result for a completed one-shot worker turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnEnvelope {
    pub protocol_version: u32,
    pub affinity_id: String,
    pub turn_id: String,
    pub worker_id: String,
    pub outcome: TurnOutcome,
}

/// Successful or failed worker execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TurnOutcome {
    Result(TurnResult),
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnResult {
    pub response: String,
    /// Backend-assigned session id for the resulting conversation.
    pub backend_session_id: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// Names of files the agent marked `[attachment:...]`, stored under the
    /// turn's `out/` key. Defaulted so a result from an older worker still
    /// deserializes.
    #[serde(default)]
    pub produced_files: Vec<String>,
}

/// Where the router lands files a worker produced, one directory per turn.
/// Kept apart from inbound attachments so an outbound file cannot overwrite
/// one of the same name.
pub fn outbound_dir(base: &std::path::Path) -> std::path::PathBuf {
    base.join("internal/attachments/outbound")
}

/// Paths named by `[attachment:/path/to/file]` markers, in order, whether or
/// not they exist on this machine.
pub fn attachment_markers(text: &str) -> Vec<String> {
    const OPEN: &str = "[attachment:";
    let mut out = Vec::new();
    for (idx, _) in text.match_indices(OPEN) {
        let start = idx + OPEN.len();
        let Some(end) = text[start..].find(']') else {
            continue;
        };
        let path = text[start..start + end].trim();
        if !path.is_empty() && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

#[async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult>;
}

/// Build the configured provider. Errors when the configuration is invalid
/// (e.g. `provider = subprocess` without a store).
pub fn try_default_provider(config: &Config, paths: &Paths) -> Result<Box<dyn SandboxProvider>> {
    use crate::config::ProviderKind;

    let store = state::default_store(config, paths)?;

    let timing = worker::Timing {
        idle: std::time::Duration::from_secs(config.deployment.worker_idle_secs),
        start_timeout: std::time::Duration::from_secs(config.deployment.worker_start_timeout_secs),
        turn_timeout: std::time::Duration::from_secs(config.deployment.turn_timeout_secs),
        max_age: std::time::Duration::from_secs(config.deployment.worker_max_age_secs),
        ..Default::default()
    };
    let policy_hash = config.deployment.policy_hash();
    let worker_cap = config.deployment.worker_cap;
    match config.deployment.provider.unwrap_or(ProviderKind::Local) {
        ProviderKind::Local => {
            let local = LocalProcessProvider::new(config.clone(), paths.clone());
            match store {
                Some(store) => Ok(Box::new(hydrating::HydratingProvider::new(
                    local,
                    store,
                    paths.claude_home.clone(),
                    paths.cursor_home.clone(),
                    paths.base.clone(),
                ))),
                None => Ok(Box::new(local)),
            }
        }
        ProviderKind::Subprocess => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = subprocess` requires [deployment].store to be set")
            })?;
            let self_exe = std::env::current_exe()?;
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(worker::SubprocessLauncher::new(self_exe, paths.clone())),
                paths.base.clone(),
                timing,
                policy_hash,
                worker_cap,
            )))
        }
        ProviderKind::Docker => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = docker` requires [deployment].store to be set")
            })?;
            let image = config
                .deployment
                .docker_image
                .clone()
                .unwrap_or_else(|| "cica-worker:latest".to_string());
            let state_store_dir = state::resolved_state_path(config, paths);
            let launcher = worker::DockerLauncher::new(
                image,
                paths.config_file.clone(),
                config.skills.is_none().then(|| paths.skills_dir.clone()),
                state_store_dir,
            );
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(launcher),
                paths.base.clone(),
                timing,
                policy_hash,
                worker_cap,
            )))
        }
        ProviderKind::Fargate => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = fargate` requires [deployment].store to be set")
            })?;
            #[cfg(feature = "fargate")]
            {
                let fc = config.deployment.fargate.clone().ok_or_else(|| {
                    anyhow::anyhow!("`provider = fargate` requires a [deployment.fargate] section")
                })?;
                Ok(Box::new(worker::LaunchedWorkerProvider::new(
                    store,
                    Box::new(fargate::FargateLauncher::new(fc)),
                    paths.base.clone(),
                    timing,
                    policy_hash,
                    worker_cap,
                )))
            }
            #[cfg(not(feature = "fargate"))]
            {
                let _ = store;
                anyhow::bail!(
                    "`provider = fargate` requires the binary to be built with `--features fargate`"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_round_trip_preserves_the_provider_destination() {
        let value = serde_json::json!({
            "channel": "telegram", "user_id": "1", "prompt": "hi",
            "system_prompt": null, "resume_session": null,
            "skip_permissions": true, "backend": "claude", "model": null,
            "claude_target": {"provider": "vertex", "project": "router-project", "region": "europe-west1"}
        });
        let job: TurnJob = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(job).unwrap()["claude_target"],
            value["claude_target"]
        );
    }

    #[test]
    fn router_resolves_destination_precedence_without_serializing_credentials() {
        let mut config = Config::default();
        config.claude.api_key = Some("secret-key".into());
        config.claude.vertex_credentials_path = Some("secret-path".into());
        config.claude.vertex_project_id = Some("router-project".into());
        config.claude.bedrock_region = Some("eu-central-1".into());
        for (bedrock, vertex, expected) in [
            (
                true,
                true,
                serde_json::json!({"provider": "bedrock", "region": "eu-central-1"}),
            ),
            (
                false,
                true,
                serde_json::json!({"provider": "vertex", "project": "router-project", "region": "europe-west1"}),
            ),
            (false, false, serde_json::json!({"provider": "anthropic"})),
        ] {
            config.claude.use_bedrock = bedrock;
            config.claude.use_vertex = vertex;
            let job = TurnJob::new(
                &config,
                "telegram",
                "1",
                Affinity::Cron {
                    job_id: "cron".into(),
                },
                "hi".into(),
                None,
                None,
            );
            let value = serde_json::to_value(job).unwrap();
            assert_eq!(value["claude_target"], expected);
            assert!(!value.to_string().contains("secret-"));
        }
    }

    #[test]
    fn subprocess_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        // No store configured → must be an error, not a silent local fallback.
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn subprocess_provider_built_when_store_present() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-prov-test".into());
        assert!(try_default_provider(&cfg, &paths).is_ok());
    }

    #[test]
    fn docker_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn docker_provider_built_when_store_present() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-docker-test".into());
        assert!(try_default_provider(&cfg, &paths).is_ok());
    }

    #[cfg(not(feature = "fargate"))]
    #[test]
    fn fargate_provider_requires_feature() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-fargate-test".into());
        // Feature off → must error even though a store is present.
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn fargate_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[cfg(feature = "fargate")]
    #[test]
    fn fargate_provider_built_when_feature_and_store_and_section() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, FargateConfig, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-fargate-test2".into());
        cfg.deployment.fargate = Some(FargateConfig {
            cluster: "cica".into(),
            task_definition: "cica-worker".into(),
            ..Default::default()
        });
        // Lazy ECS client: building the provider does not connect.
        assert!(try_default_provider(&cfg, &paths).is_ok());
    }

    #[test]
    fn turn_job_and_result_round_trip_json() {
        let job = TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            affinity: Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: SessionPersistence::Resume,
            prompt: "hi".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            skip_permissions: true,
            backend: crate::config::AiBackend::Claude,
            model: None,
            claude_target: None,
            attachments: Vec::new(),
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: TurnJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.channel, "telegram");
        assert_eq!(back.resume_session.as_deref(), Some("sess-1"));

        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess-2".into(),
            cost_usd: Some(0.1),
            duration_ms: Some(5),
            produced_files: Vec::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TurnResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend_session_id, "sess-2");
    }

    #[test]
    fn new_job_takes_backend_and_model_from_config() {
        let mut cfg = Config {
            backend: AiBackend::Claude,
            ..Default::default()
        };
        cfg.claude.model = Some("opus".into());
        cfg.cursor.model = Some("auto".into());
        let job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        assert_eq!(job.backend, AiBackend::Claude);
        assert_eq!(job.model.as_deref(), Some("opus"));
        assert!(job.skip_permissions);

        cfg.backend = AiBackend::Cursor;
        let job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        assert_eq!(job.backend, AiBackend::Cursor);
        assert_eq!(job.model.as_deref(), Some("auto"));
        assert!(job.skip_permissions);
    }

    #[test]
    fn turn_job_ignores_fields_an_older_router_sends() {
        let json = r#"{
            "session_id":"telegram:1","channel":"telegram","user_id":"1",
            "prompt":"hi","system_prompt":null,"resume_session":"sess-1",
            "cwd":"/tmp/work","skip_permissions":true,"backend":"claude","model":null
        }"#;
        let job: TurnJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.channel, "telegram");
        assert_eq!(job.resume_session.as_deref(), Some("sess-1"));
        assert_eq!(job.claude_target, None);
    }

    #[test]
    fn affinity_encoding_distinguishes_punctuation() {
        let left = Affinity::Chat {
            channel: "a:b".into(),
            user: "c".into(),
        };
        let right = Affinity::Chat {
            channel: "a".into(),
            user: "b:c".into(),
        };
        assert_ne!(left.id(), right.id());
    }

    #[test]
    fn slack_affinity_includes_channel() {
        let left = Affinity::SlackThread {
            channel_id: "C1".into(),
            thread_ts: "1.2".into(),
        };
        let right = Affinity::SlackThread {
            channel_id: "C2".into(),
            thread_ts: "1.2".into(),
        };
        assert_ne!(left.id(), right.id());
    }

    #[test]
    fn old_job_defaults_to_chat_affinity() {
        let json = r#"{"channel":"slack","user_id":"U1","prompt":"hi","system_prompt":null,
                      "resume_session":null,"skip_permissions":true,"backend":"claude","model":null}"#;
        let job: TurnJob = serde_json::from_str(json).unwrap();
        assert_eq!(
            job.affinity,
            Affinity::Chat {
                channel: "slack".into(),
                user: "U1".into()
            }
        );
        assert_eq!(job.session_persistence, SessionPersistence::Resume);
    }
}

#[cfg(test)]
mod attachment_compat_tests {
    use super::*;

    #[test]
    fn a_job_written_by_an_older_router_still_deserializes() {
        let old = r#"{"channel":"slack","user_id":"U1","prompt":"hi","system_prompt":null,
                      "resume_session":null,"skip_permissions":true,"backend":"claude","model":null}"#;
        let job: TurnJob = serde_json::from_str(old).expect("old job deserializes");
        assert!(job.attachments.is_empty());
    }

    #[test]
    fn attachments_survive_a_round_trip() {
        let old = r#"{"channel":"slack","user_id":"U1","prompt":"hi","system_prompt":null,
                      "resume_session":null,"skip_permissions":true,"backend":"claude","model":null}"#;
        let job: TurnJob = serde_json::from_str(old).unwrap();
        let job = job.with_attachments(vec!["internal/slack_attachments/F1_shot.png".into()]);
        let back: TurnJob = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        assert_eq!(
            back.attachments,
            vec!["internal/slack_attachments/F1_shot.png".to_string()]
        );
    }
}
