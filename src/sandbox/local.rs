//! Local-subprocess sandbox provider (Phase 1: today's behavior).

use anyhow::Result;
use async_trait::async_trait;

use crate::backends::{self, QueryResult};
use crate::config::{Config, Paths};
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Runs an agent turn in a local subprocess (today's behavior).
pub struct LocalProcessProvider {
    config: Config,
    paths: Paths,
    dispatched: bool,
}

impl LocalProcessProvider {
    pub fn new(config: Config, paths: Paths) -> Self {
        Self {
            config,
            paths,
            dispatched: false,
        }
    }

    pub fn for_worker(config: Config, paths: Paths) -> Self {
        Self {
            config,
            paths,
            dispatched: true,
        }
    }
}

#[async_trait]
impl SandboxProvider for LocalProcessProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        // Make sure the per-user memories dir exists so the agent can write into it.
        let dir = crate::memory::memories_dir(&self.paths, &job.channel, &job.user_id);
        let _ = std::fs::create_dir_all(&dir);
        let mut options = job_to_query_options(&self.paths, &job);
        options.dispatched = self.dispatched;
        let qr = backends::query_with_options(
            job.backend,
            &self.config,
            &self.paths,
            &job.prompt,
            options,
        )
        .await?;
        Ok(turn_result_from_query(qr))
    }
}

/// Resolve `{MEMORIES_DIR}` in the system prompt to the given local memories
/// path. Token absent → prompt returned unchanged; `None` prompt → `None`.
fn substitute_token(system_prompt: Option<&str>, memories_dir: &std::path::Path) -> Option<String> {
    let sp = system_prompt?;
    Some(sp.replace(
        crate::memory::MEMORIES_DIR_TOKEN,
        &memories_dir.to_string_lossy(),
    ))
}

fn job_to_query_options(paths: &Paths, job: &TurnJob) -> backends::QueryOptions {
    let dir = crate::memory::memories_dir(paths, &job.channel, &job.user_id);
    let system_prompt = substitute_token(job.system_prompt.as_deref(), &dir);
    backends::QueryOptions {
        system_prompt,
        resume_session: job.resume_session.clone(),
        skip_permissions: job.skip_permissions,
        model: job.model.clone(),
        claude_target: job.claude_target.clone(),
        dispatched: false,
    }
}

pub(crate) fn turn_result_from_query(qr: QueryResult) -> TurnResult {
    TurnResult {
        response: qr.response,
        backend_session_id: qr.session_id,
        cost_usd: qr.cost_usd,
        duration_ms: qr.duration_ms,
        // A local run writes files where the channel can already read them.
        produced_files: Vec::new(),
    }
}

pub fn query_result_from_turn(tr: TurnResult) -> QueryResult {
    QueryResult {
        response: tr.response,
        session_id: tr.backend_session_id,
        duration_ms: tr.duration_ms,
        cost_usd: tr.cost_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiBackend;
    use std::path::Path;

    fn sample_job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "42".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "42".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
            prompt: "hello".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: Some("claude-opus-4-6".into()),
            claude_target: None,
            attachments: Vec::new(),
        }
    }

    fn recording_claude(paths: &Paths) {
        use std::os::unix::fs::PermissionsExt;
        let cli = paths.claude_code_dir.join("node_modules/.bin/claude");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&paths.base).unwrap();
        std::fs::write(&cli, "#!/bin/sh\n/usr/bin/env > child-env\nprintf '%s\\n' \"$@\" > child-args\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"ok\",\"session_id\":\"s\"}'\n").unwrap();
        std::fs::set_permissions(cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn dispatched_bedrock_target_and_default_model_override_worker_config() {
        let (_temp, paths) = crate::config::test_paths();
        recording_claude(&paths);
        let mut config = Config::default();
        config.claude.model = Some("old-worker-model".into());
        let provider = LocalProcessProvider::for_worker(config, paths.clone());
        let mut job = sample_job();
        job.model = None;
        job.claude_target = Some(crate::sandbox::ClaudeTarget::Bedrock {
            region: Some("eu-central-1".into()),
        });
        assert_eq!(provider.run_turn(job).await.unwrap().response, "ok");
        let env = std::fs::read_to_string(paths.base.join("child-env")).unwrap();
        assert!(env.lines().any(|line| line == "CLAUDE_CODE_USE_BEDROCK=1"));
        assert!(env.lines().any(|line| line == "AWS_REGION=eu-central-1"));
        assert!(env.lines().any(|line| line.starts_with("AWS_CONFIG_FILE=")));
        assert!(
            env.lines()
                .any(|line| line.starts_with("AWS_SHARED_CREDENTIALS_FILE="))
        );
        let args = std::fs::read_to_string(paths.base.join("child-args")).unwrap();
        assert!(args.contains("--model\ndefault\n"));
        assert!(!args.contains("old-worker-model"));
    }

    #[tokio::test]
    async fn legacy_job_uses_workers_vertex_destination_and_credentials() {
        let (_temp, paths) = crate::config::test_paths();
        recording_claude(&paths);
        let credentials = paths.base.join("worker-credentials.json");
        std::fs::write(&credentials, "{}").unwrap();
        let mut config = Config::default();
        config.claude.use_vertex = true;
        config.claude.vertex_project_id = Some("worker-project".into());
        config.claude.vertex_credentials_path = Some(credentials.display().to_string());
        let provider = LocalProcessProvider::for_worker(config, paths.clone());
        provider.run_turn(sample_job()).await.unwrap();
        let env = std::fs::read_to_string(paths.base.join("child-env")).unwrap();
        assert!(
            env.lines()
                .any(|line| line == "ANTHROPIC_VERTEX_PROJECT_ID=worker-project")
        );
        assert!(env.lines().any(
            |line| line == format!("GOOGLE_APPLICATION_CREDENTIALS={}", credentials.display())
        ));
    }

    #[tokio::test]
    async fn inherited_routing_overrides_do_not_reach_an_anthropic_job() {
        const MARKER: &str = "CICA_TEST_ROUTING_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sandbox::local::tests::inherited_routing_overrides_do_not_reach_an_anthropic_job", "--nocapture"])
                .env(MARKER, "1")
                .env("CLAUDE_CODE_USE_BEDROCK", "1")
                .env("CLAUDE_CODE_USE_VERTEX", "1")
                .env("CLAUDE_CODE_USE_FOUNDRY", "1")
                .env("ANTHROPIC_BASE_URL", "https://wrong.example")
                .env("ANTHROPIC_BEDROCK_BASE_URL", "https://wrong.example")
                .env("ANTHROPIC_VERTEX_BASE_URL", "https://wrong.example")
                .env("ANTHROPIC_AUTH_TOKEN", "stale-auth")
                .env("AWS_BEARER_TOKEN_BEDROCK", "stale-aws-auth")
                .env("CLAUDE_CODE_SKIP_BEDROCK_AUTH", "1")
                .env("ANTHROPIC_MODEL", "stale-model")
                .env("VERTEX_REGION_CLAUDE_4_6_SONNET", "us-east5")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let (_temp, paths) = crate::config::test_paths();
        recording_claude(&paths);
        let mut config = Config::default();
        config.claude.use_bedrock = true;
        config.claude.api_key = Some("sk-ant-api03-worker-key".into());
        let provider = LocalProcessProvider::for_worker(config, paths.clone());
        let mut job = sample_job();
        job.claude_target = Some(crate::sandbox::ClaudeTarget::Anthropic);
        provider.run_turn(job).await.unwrap();
        let env = std::fs::read_to_string(paths.base.join("child-env")).unwrap();
        assert!(
            env.lines()
                .any(|line| line == "ANTHROPIC_API_KEY=sk-ant-api03-worker-key")
        );
        assert!(
            env.lines()
                .any(|line| line == "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1")
        );
        for name in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "ANTHROPIC_MODEL",
            "VERTEX_REGION_CLAUDE_4_6_SONNET",
        ] {
            assert!(
                !env.lines()
                    .any(|line| line.starts_with(&format!("{name}="))),
                "{name} reached the child"
            );
        }
    }

    #[tokio::test]
    async fn dispatched_vertex_uses_job_project_and_worker_credential_path() {
        let (_temp, paths) = crate::config::test_paths();
        recording_claude(&paths);
        let credentials = paths.base.join("credentials.json");
        std::fs::write(&credentials, "{}").unwrap();
        let mut config = Config::default();
        config.claude.use_bedrock = true;
        config.claude.vertex_project_id = Some("old-project".into());
        config.claude.vertex_credentials_path = Some(credentials.display().to_string());
        let provider = LocalProcessProvider::for_worker(config, paths.clone());
        let mut job = sample_job();
        job.claude_target = Some(crate::sandbox::ClaudeTarget::Vertex {
            project: "job-project".into(),
            region: "europe-west4".into(),
        });
        provider.run_turn(job).await.unwrap();
        let env = std::fs::read_to_string(paths.base.join("child-env")).unwrap();
        assert!(
            env.lines()
                .any(|line| line == "ANTHROPIC_VERTEX_PROJECT_ID=job-project")
        );
        assert!(
            env.lines()
                .any(|line| line == "CLOUD_ML_REGION=europe-west4")
        );
        assert!(env.lines().any(
            |line| line == format!("GOOGLE_APPLICATION_CREDENTIALS={}", credentials.display())
        ));
        assert!(
            !env.lines()
                .any(|line| line.starts_with("ANTHROPIC_API_KEY="))
        );
    }

    #[tokio::test]
    async fn dispatched_jobs_reject_settings_that_remap_the_model() {
        for location in ["user", "project", "local"] {
            for setting in [
                serde_json::json!({"modelOverrides": {"claude-opus-4-6": "wrong-provider-model"}}),
                serde_json::json!({"fallbackModel": ["wrong-provider-model"]}),
            ] {
                let (_temp, paths) = crate::config::test_paths();
                recording_claude(&paths);
                let settings = match location {
                    "user" => paths.claude_home.join(".claude/settings.json"),
                    "project" => paths.base.join(".claude/settings.json"),
                    _ => paths.base.join(".claude/settings.local.json"),
                };
                std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
                std::fs::write(&settings, serde_json::to_vec(&setting).unwrap()).unwrap();
                let mut config = Config::default();
                config.claude.api_key = Some("worker-key".into());
                let provider = LocalProcessProvider::for_worker(config.clone(), paths.clone());
                let mut job = sample_job();
                job.claude_target = Some(crate::sandbox::ClaudeTarget::Anthropic);
                let error = provider.run_turn(job.clone()).await.unwrap_err();
                assert!(error.to_string().contains("job-selected model"));
                assert!(!paths.base.join("child-env").exists());
                LocalProcessProvider::new(config, paths)
                    .run_turn(job)
                    .await
                    .unwrap();
            }
        }
    }

    #[test]
    fn job_maps_to_query_options() {
        let (_temp, paths) = crate::config::test_paths();
        let job = sample_job();
        let opts = job_to_query_options(&paths, &job);
        assert_eq!(opts.system_prompt.as_deref(), Some("ctx"));
        assert_eq!(opts.resume_session.as_deref(), Some("sess-1"));
        assert!(opts.skip_permissions);
        assert_eq!(opts.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn query_result_maps_to_turn_result() {
        let qr = QueryResult {
            response: "hi".into(),
            session_id: "sess-9".into(),
            duration_ms: Some(123),
            cost_usd: Some(0.5),
        };
        let tr = turn_result_from_query(qr);
        assert_eq!(tr.response, "hi");
        assert_eq!(tr.backend_session_id, "sess-9");
        assert_eq!(tr.duration_ms, Some(123));
        assert_eq!(tr.cost_usd, Some(0.5));
    }

    #[test]
    fn turn_result_maps_back_to_query_result() {
        let tr = TurnResult {
            response: "yo".into(),
            backend_session_id: "sess-3".into(),
            cost_usd: None,
            duration_ms: None,
            produced_files: Vec::new(),
        };
        let qr = query_result_from_turn(tr);
        assert_eq!(qr.response, "yo");
        assert_eq!(qr.session_id, "sess-3");
    }

    #[test]
    fn provider_is_constructible_and_object_safe() {
        let (_temp, paths) = crate::config::test_paths();
        let p = LocalProcessProvider::new(Config::default(), paths);
        let _boxed: Box<dyn crate::sandbox::SandboxProvider> = Box::new(p);
    }

    #[test]
    fn substitutes_memories_token_when_present() {
        let out = substitute_token(
            Some("save to {MEMORIES_DIR}/x.md please"),
            Path::new("/data/cica/users/telegram_1/memories"),
        );
        assert_eq!(
            out.as_deref(),
            Some("save to /data/cica/users/telegram_1/memories/x.md please")
        );
    }

    #[test]
    fn leaves_prompt_unchanged_when_token_absent() {
        let out = substitute_token(Some("no token here"), Path::new("/m"));
        assert_eq!(out.as_deref(), Some("no token here"));
    }

    #[test]
    fn none_prompt_stays_none() {
        let out = substitute_token(None, Path::new("/m"));
        assert_eq!(out, None);
    }

    #[test]
    fn job_options_substitutes_memories_token() {
        let (_temp, paths) = crate::config::test_paths();
        let mut job = sample_job();
        job.system_prompt = Some("write to {MEMORIES_DIR}/notes.md".into());
        let opts = job_to_query_options(&paths, &job);
        let sp = opts.system_prompt.unwrap();
        assert!(!sp.contains("{MEMORIES_DIR}"));
        assert!(sp.contains("/notes.md"));
    }

    #[tokio::test]
    async fn job_backend_selects_cursor_on_a_claude_config() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config {
            backend: AiBackend::Claude,
            ..Default::default()
        };
        cfg.claude.api_key = Some("k".into());
        cfg.cursor.api_key = None;
        let provider = LocalProcessProvider::new(cfg.clone(), paths);
        let mut job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        job.backend = AiBackend::Cursor;
        let error = provider.run_turn(job).await.unwrap_err().to_string();
        assert!(error.contains("No Cursor API key configured"));
        assert!(!error.contains("Claude"));
    }

    #[tokio::test]
    async fn job_backend_selects_claude_on_a_cursor_config() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config {
            backend: AiBackend::Cursor,
            ..Default::default()
        };
        cfg.cursor.api_key = Some("k".into());
        cfg.claude.api_key = None;
        let provider = LocalProcessProvider::new(cfg.clone(), paths);
        let mut job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        job.backend = AiBackend::Claude;
        let error = provider.run_turn(job).await.unwrap_err().to_string();
        assert!(error.contains("No credential configured"));
    }

    #[test]
    fn job_model_reaches_query_options() {
        let (_temp, paths) = crate::config::test_paths();
        let mut job = sample_job();
        job.model = Some("claude-opus-4-6".into());
        assert_eq!(
            job_to_query_options(&paths, &job).model.as_deref(),
            Some("claude-opus-4-6")
        );
        job.model = None;
        assert_eq!(job_to_query_options(&paths, &job).model, None);
    }
}
