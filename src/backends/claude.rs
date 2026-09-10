//! Claude Code integration

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::backends::{QueryOptions, QueryResult};
use crate::config::{ClaudeConfig, Paths};
use crate::sandbox::ClaudeTarget;
use crate::setup;

pub const MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
];

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    #[serde(rename = "type")]
    response_type: String,
    result: Option<String>,
    session_id: Option<String>,
    duration_ms: Option<u64>,
    total_cost_usd: Option<f64>,
    /// Keyed by the concrete model ID served; the only place an alias like "opus" resolves.
    #[serde(rename = "modelUsage", default)]
    model_usage: Option<serde_json::Map<String, serde_json::Value>>,
}

fn served_models(usage: &Option<serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let usage = usage.as_ref()?;
    if usage.is_empty() {
        return None;
    }
    let mut ids: Vec<&str> = usage.keys().map(String::as_str).collect();
    ids.sort_unstable();
    Some(ids.join(", "))
}

// Claude Code's wording: "No conversation found with session ID: <uuid>".
fn is_missing_conversation(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no conversation found")
}

fn config_relative_path(paths: &Paths, value: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(value);
    if path.is_relative() {
        paths.config_file.parent().unwrap_or(&paths.base).join(path)
    } else {
        path.to_path_buf()
    }
}

/// Point Claude Code at the target, and report whether cica handed it a
/// credential for that target.
#[must_use]
fn apply_backend_env(
    cmd: &mut Command,
    claude: &ClaudeConfig,
    paths: &Paths,
    target: &ClaudeTarget,
) -> bool {
    match target {
        ClaudeTarget::Bedrock { region } => {
            cmd.env("CLAUDE_CODE_USE_BEDROCK", "1");
            if let Some(region) = region {
                cmd.env("AWS_REGION", region);
            }
            for name in [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ] {
                cmd.env_remove(name);
            }
            // Bedrock credentials come from the AWS chain; cica never sees them.
            false
        }
        ClaudeTarget::Vertex { project, region } => {
            cmd.env("CLAUDE_CODE_USE_VERTEX", "1")
                .env("ANTHROPIC_VERTEX_PROJECT_ID", project)
                .env("CLOUD_ML_REGION", region);
            // A service-account file is cica's to supply; gcloud ADC is the ambient chain.
            let mut supplied = false;
            if let Some(ref cred_path) = claude.vertex_credentials_path {
                let abs = config_relative_path(paths, cred_path);
                if abs.exists() {
                    cmd.env("GOOGLE_APPLICATION_CREDENTIALS", &abs);
                    supplied = true;
                }
            }
            supplied
        }
        ClaudeTarget::Anthropic => {
            if let Some(cred) = claude.api_key.as_deref() {
                match setup::detect_credential_type(cred) {
                    setup::CredentialType::ApiKey => {
                        cmd.env("ANTHROPIC_API_KEY", cred);
                    }
                    setup::CredentialType::OAuthToken => {
                        cmd.env("CLAUDE_CODE_OAUTH_TOKEN", cred);
                        cmd.env("ANTHROPIC_OAUTH_TOKEN", cred);
                    }
                }
                true
            } else {
                false
            }
        }
    }
}

fn competing_env(name: &str) -> bool {
    name.starts_with("ANTHROPIC_")
        || name.starts_with("VERTEX_REGION_CLAUDE_")
        || name.starts_with("AWS_ENDPOINT_URL")
        || matches!(
            name,
            "CLAUDE_CODE_USE_BEDROCK"
                | "CLAUDE_CODE_USE_VERTEX"
                | "CLAUDE_CODE_USE_FOUNDRY"
                | "CLAUDE_CODE_USE_ANTHROPIC_AWS"
                | "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD"
                | "CLAUDE_CODE_USE_MANTLE"
                | "CLAUDE_CODE_USE_GATEWAY"
                | "CLAUDE_CODE_SKIP_BEDROCK_AUTH"
                | "CLAUDE_CODE_SKIP_VERTEX_AUTH"
                | "CLAUDE_CODE_SKIP_FOUNDRY_AUTH"
                | "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH"
                | "CLAUDE_CODE_SKIP_ANTHROPIC_GOOGLE_CLOUD_AUTH"
                | "CLAUDE_CODE_SKIP_MANTLE_AUTH"
                | "CLAUDE_CODE_OAUTH_TOKEN"
                | "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR"
                | "CLAUDE_CODE_HOST_AUTH_ENV_VAR"
                | "CLAUDE_CODE_HOST_CREDS_FILE"
                | "CLAUDE_CODE_API_BASE_URL"
                | "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"
                | "CLAUDE_CODE_SUBAGENT_MODEL"
                | "CLAUDE_CODE_SUBAGENT_MODEL_FORCE"
                | "CLAUDE_CODE_AUTO_MODE_MODEL"
                | "CLAUDE_CODE_BG_CLASSIFIER_MODEL"
                | "CLAUDE_CONTEXT_COLLAPSE_MODEL"
                | "CLAUDE_CODE_EXTRA_BODY"
                | "CLOUD_ML_REGION"
                | "AWS_REGION"
                | "AWS_DEFAULT_REGION"
        )
}

fn isolate_backend_env(cmd: &mut Command, target: &ClaudeTarget) {
    let names = std::env::vars_os()
        .map(|(key, _)| key)
        .chain(cmd.as_std().get_envs().map(|(key, _)| key.to_os_string()))
        .collect::<Vec<_>>();
    for name in names {
        if competing_env(&name.to_string_lossy()) {
            cmd.env_remove(name);
        }
    }
    if !matches!(target, ClaudeTarget::Bedrock { .. }) {
        cmd.env_remove("AWS_BEARER_TOKEN_BEDROCK");
    }
    if !matches!(target, ClaudeTarget::Vertex { .. }) {
        cmd.env_remove("GOOGLE_APPLICATION_CREDENTIALS");
    }
    // Claude Code must also ignore endpoints supplied by user/project/managed settings.
    cmd.env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true");
}

async fn reject_model_routing_settings(paths: &Paths) -> Result<()> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.claude_home.join(".claude"));
    let config_dir = if config_dir.is_absolute() {
        config_dir
    } else {
        paths.base.join(config_dir)
    };
    let mut files = vec![
        config_dir.join("settings.json"),
        config_dir.join("cowork_settings.json"),
        paths.base.join(".claude/settings.json"),
        paths.base.join(".claude/settings.local.json"),
    ];
    let base = std::fs::canonicalize(&paths.base)?;
    if let Some(root) = base.ancestors().find(|dir| dir.join(".git").exists()) {
        files.push(root.join(".claude/settings.local.json"));
        // Claude also reads the main worktree's local settings from linked worktrees.
        let output = Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(&base)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .kill_on_drop(true)
            .output()
            .await
            .context("finding Claude's shared local settings")?;
        anyhow::ensure!(
            output.status.success(),
            "could not locate Claude's shared local settings"
        );
        let common = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        let root = if common.file_name().is_some_and(|name| name == ".git") {
            common
                .parent()
                .context("git common directory has no parent")?
        } else {
            &common
        };
        files.push(root.join(".claude/settings.local.json"));
    }
    for file in files {
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
        };
        let settings: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", file.display()))?;
        for name in ["modelOverrides", "fallbackModel"] {
            if let Some(value) = settings.get(name) {
                anyhow::ensure!(
                    value.is_null()
                        || value.as_object().is_some_and(|v| v.is_empty())
                        || value.as_array().is_some_and(|v| v.is_empty()),
                    "{} in {} conflicts with the job-selected model; remove it for distributed turns",
                    name,
                    file.display()
                );
            }
        }
    }
    Ok(())
}

fn apply_aws_file_env(
    cmd: &mut Command,
    original_home: &std::path::Path,
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) {
    for (name, filename) in [
        ("AWS_CONFIG_FILE", "config"),
        ("AWS_SHARED_CREDENTIALS_FILE", "credentials"),
    ] {
        let value =
            get(name).unwrap_or_else(|| original_home.join(".aws").join(filename).into_os_string());
        cmd.env(name, value);
    }
}

fn prepare_bedrock_home(paths: &Paths, original_home: &std::path::Path) -> Result<()> {
    let aws = original_home.join(".aws");
    if !aws.is_dir() || paths.claude_home == original_home {
        return Ok(());
    }
    std::fs::create_dir_all(&paths.claude_home)?;
    let link = paths.claude_home.join(".aws");
    // The AWS SDK resolves the SSO cache through HOME even with explicit shared-file paths.
    match std::os::unix::fs::symlink(&aws, &link) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                std::fs::read_link(&link).ok().as_ref() == Some(&aws),
                "{} already exists and does not link to {}",
                link.display(),
                aws.display()
            );
            Ok(())
        }
        Err(error) => Err(error).context("linking the original AWS directory into Claude's home"),
    }
}

pub async fn query_with_options(
    claude: &ClaudeConfig,
    paths: &Paths,
    prompt: &str,
    options: QueryOptions,
) -> Result<QueryResult> {
    let target = options
        .claude_target
        .clone()
        .unwrap_or_else(|| ClaudeTarget::from_config(claude));
    let authoritative = options.dispatched && options.claude_target.is_some();
    if authoritative {
        target.validate_distributed()?;
        reject_model_routing_settings(paths).await?;
    }
    match &target {
        ClaudeTarget::Bedrock { region } => debug!(?region, "Using Amazon Bedrock"),
        ClaudeTarget::Vertex { project, .. } => {
            anyhow::ensure!(
                !project.is_empty(),
                "Vertex AI is enabled but no project ID is set. Run `cica init` to configure Vertex AI."
            );
            debug!(project, "Using Vertex AI");
        }
        ClaudeTarget::Anthropic => {
            claude.api_key.as_deref().ok_or_else(|| {
                anyhow!("No credential configured. Run `cica init` to set up Claude.")
            })?;
        }
    }

    let claude_code = setup::find_claude_code(paths)
        .ok_or_else(|| anyhow!("Claude Code not found. Run `cica init` to set up Claude."))?;

    let (program, prefix_args): (PathBuf, Vec<PathBuf>) = match &claude_code {
        setup::ClaudeCode::Native(exe) => (exe.clone(), Vec::new()),
        setup::ClaudeCode::Script(js) => {
            let bun = setup::find_bun(paths)
                .ok_or_else(|| anyhow!("Bun not found. Run `cica init` to set up Claude."))?;
            (bun, vec![PathBuf::from("run"), js.clone()])
        }
    };

    match options.model.as_deref() {
        Some(model) => info!("Claude model requested: {}", model),
        None => info!("Claude model requested: none configured, using the CLI default"),
    }

    info!("Querying Claude: {}", prompt);
    debug!("Using claude_code: {:?}", claude_code);

    let aws_home = if matches!(target, ClaudeTarget::Bedrock { .. }) {
        let home =
            std::env::home_dir().context("Could not determine the original AWS home directory")?;
        let home = std::path::absolute(home)?;
        prepare_bedrock_home(paths, &home)?;
        Some(home)
    } else {
        None
    };

    let build_command = |resume_session: Option<&str>| {
        let mut cmd = Command::new(&program);
        cmd.args(&prefix_args)
            .args(["-p", "--output-format", "json"])
            .env("HOME", &paths.claude_home);

        if options.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }

        if let Some(ref system_prompt) = options.system_prompt {
            if resume_session.is_none() {
                cmd.args(["--system-prompt", system_prompt]);
            } else {
                cmd.args(["--append-system-prompt", system_prompt]);
            }
        }

        if let Some(session_id) = resume_session {
            cmd.args(["--resume", session_id]);
        }

        if let Some(ref model) = options.model {
            cmd.args(["--model", model]);
        } else if options.dispatched {
            cmd.args(["--model", "default"]);
        }

        cmd.current_dir(&paths.base);
        cmd.kill_on_drop(true);
        cmd.as_std_mut().process_group(0);

        cmd.arg(prompt);

        if authoritative {
            isolate_backend_env(&mut cmd, &target);
        }
        let host_credential = apply_backend_env(&mut cmd, claude, paths, &target);
        // Claude Code reads this flag as "the host supplies credentials" and stops
        // consulting ambient providers, so claiming it for a role-based Bedrock or
        // ADC Vertex turn leaves that turn no way to authenticate.
        if authoritative && host_credential {
            cmd.env("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1");
        }
        if let Some(home) = &aws_home {
            apply_aws_file_env(&mut cmd, home, |name| std::env::var_os(name));
        }

        cmd
    };

    let mut resume = options.resume_session.clone();
    let output = loop {
        let mut command = build_command(resume.as_deref());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let mut group =
            crate::backends::ProcessGroupGuard::new(child.id().expect("spawned child has pid"));
        let output = child.wait_with_output().await?;
        group.disarm();

        if output.status.success() {
            break output;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if let Some(lost) = resume.take().filter(|_| is_missing_conversation(&stderr)) {
            warn!(
                "Session {} no longer exists; starting a fresh session and losing its history",
                lost
            );
            continue;
        }

        warn!("Claude CLI failed. stdout: {}", stdout);
        warn!("Claude CLI failed. stderr: {}", stderr);
        bail!(
            "Claude CLI failed (exit {:?}): {}{}",
            output.status.code(),
            stderr,
            if stderr.is_empty() { &stdout } else { "" }
        );
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    debug!("Claude raw output: {}", stdout);

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(response) = serde_json::from_str::<ClaudeResponse>(line) else {
            continue;
        };

        if response.response_type == "result"
            && let Some(result) = response.result
        {
            info!(
                "Claude response received ({}ms, ${:.4}, served by {})",
                response.duration_ms.unwrap_or(0),
                response.total_cost_usd.unwrap_or(0.0),
                served_models(&response.model_usage).unwrap_or_else(|| "unreported".into())
            );
            return Ok(QueryResult {
                response: result,
                session_id: response.session_id.unwrap_or_default(),
                duration_ms: response.duration_ms,
                cost_usd: response.total_cost_usd,
            });
        }
    }

    Err(anyhow!("No result found in Claude output"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::process::Command;

    use super::{
        ClaudeResponse, apply_backend_env, config_relative_path, is_missing_conversation,
        isolate_backend_env, served_models,
    };
    use crate::config::{ClaudeConfig, Paths};

    /// Environment the command would hand to Claude Code. A `None` value is a
    /// removal: the child cannot inherit that variable from us.
    fn envs(cmd: &Command) -> HashMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn applied(claude: &ClaudeConfig) -> HashMap<String, Option<String>> {
        applied_with_credential(claude, &Paths::for_base("/worker".into())).0
    }

    fn applied_with_credential(
        claude: &ClaudeConfig,
        paths: &Paths,
    ) -> (HashMap<String, Option<String>>, bool) {
        let mut cmd = Command::new("claude");
        let supplied = apply_backend_env(
            &mut cmd,
            claude,
            paths,
            &crate::sandbox::ClaudeTarget::from_config(claude),
        );
        (envs(&cmd), supplied)
    }

    #[test]
    fn bedrock_supplies_no_credential_so_the_host_does_not_manage_auth() {
        let (_, supplied) = applied_with_credential(
            &ClaudeConfig {
                use_bedrock: true,
                bedrock_region: Some("eu-central-1".into()),
                ..Default::default()
            },
            &Paths::for_base("/worker".into()),
        );
        // Bedrock credentials come from the AWS chain -- an instance profile or
        // an ECS task role. Claiming host-managed auth here stops Claude Code
        // consulting that chain and the turn cannot authenticate at all.
        assert!(!supplied);
    }

    #[test]
    fn an_api_key_is_a_host_supplied_credential() {
        let (_, supplied) = applied_with_credential(
            &ClaudeConfig {
                api_key: Some("sk-ant-api03-real".into()),
                ..Default::default()
            },
            &Paths::for_base("/worker".into()),
        );
        assert!(supplied);
    }

    #[test]
    fn anthropic_without_a_key_supplies_nothing() {
        let (_, supplied) =
            applied_with_credential(&ClaudeConfig::default(), &Paths::for_base("/worker".into()));
        assert!(!supplied);
    }

    #[test]
    fn vertex_supplies_a_credential_only_with_a_service_account_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Paths::for_base(dir.path().to_path_buf());
        paths.config_file = dir.path().join("config.toml");
        let key = dir.path().join("sa.json");

        let vertex = |path: Option<&str>| ClaudeConfig {
            use_vertex: true,
            vertex_project_id: Some("a-project".into()),
            vertex_region: Some("europe-west1".into()),
            vertex_credentials_path: path.map(str::to_string),
            ..Default::default()
        };

        let (_, adc) = applied_with_credential(&vertex(None), &paths);
        assert!(!adc);

        std::fs::write(&key, "{}").unwrap();
        let (env, file) = applied_with_credential(&vertex(Some("sa.json")), &paths);
        assert!(file);
        assert!(env.contains_key("GOOGLE_APPLICATION_CREDENTIALS"));
    }

    #[test]
    fn isolation_does_not_claim_host_managed_auth_on_its_own() {
        let mut cmd = Command::new("claude");
        isolate_backend_env(
            &mut cmd,
            &crate::sandbox::ClaudeTarget::Bedrock {
                region: Some("eu-central-1".into()),
            },
        );
        let env = envs(&cmd);
        assert_eq!(
            env.get("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"),
            Some(&Some("true".into()))
        );
        assert!(!env.contains_key("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"));
    }

    #[tokio::test]
    async fn linked_worktree_cannot_inherit_a_model_remap_from_the_main_worktree() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let linked = root.path().join("linked");
        std::fs::create_dir_all(&main).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
            vec!["worktree", "add", "--detach", linked.to_str().unwrap()],
        ] {
            let output = Command::new("git")
                .current_dir(&main)
                .args(args)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let settings = main.join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            r#"{"modelOverrides":{"claude-opus-4-6":"wrong-model"}}"#,
        )
        .unwrap();
        let paths = Paths::for_base(linked);
        let error = super::reject_model_routing_settings(&paths)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("job-selected model"));
        std::fs::write(
            &settings,
            r#"{"modelOverrides":{},"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        super::reject_model_routing_settings(&paths).await.unwrap();
    }

    #[tokio::test]
    async fn separate_git_dir_cannot_hide_repository_local_model_remaps() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let metadata = root.path().join("metadata");
        let output = Command::new("git")
            .arg("init")
            .arg("--separate-git-dir")
            .arg(&metadata)
            .arg(&main)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let base = main.join("subdirectory");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(main.join(".claude")).unwrap();
        std::fs::write(
            main.join(".claude/settings.local.json"),
            r#"{"fallbackModel":["wrong-model"]}"#,
        )
        .unwrap();
        let error = super::reject_model_routing_settings(&Paths::for_base(base))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("job-selected model"));
    }

    #[test]
    fn bedrock_shared_files_point_to_original_home_and_respect_overrides() {
        let mut cmd = Command::new("claude");
        super::apply_aws_file_env(&mut cmd, std::path::Path::new("/original"), |_| None);
        let env = envs(&cmd);
        assert_eq!(env["AWS_CONFIG_FILE"], Some("/original/.aws/config".into()));
        assert_eq!(
            env["AWS_SHARED_CREDENTIALS_FILE"],
            Some("/original/.aws/credentials".into())
        );

        let mut cmd = Command::new("claude");
        super::apply_aws_file_env(&mut cmd, std::path::Path::new("/original"), |name| {
            (name == "AWS_CONFIG_FILE").then(|| std::ffi::OsString::from("/explicit/config"))
        });
        let env = envs(&cmd);
        assert_eq!(env["AWS_CONFIG_FILE"], Some("/explicit/config".into()));
        assert_eq!(
            env["AWS_SHARED_CREDENTIALS_FILE"],
            Some("/original/.aws/credentials".into())
        );

        let mut cmd = Command::new("claude");
        super::apply_aws_file_env(&mut cmd, std::path::Path::new("/original"), |_| {
            Some(std::ffi::OsString::new())
        });
        let env = envs(&cmd);
        assert_eq!(env["AWS_CONFIG_FILE"], Some(String::new()));
        assert_eq!(env["AWS_SHARED_CREDENTIALS_FILE"], Some(String::new()));
    }

    #[test]
    fn bedrock_home_shares_the_original_sso_cache_without_copying() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("original");
        let cache = home.join(".aws/sso/cache/token.json");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "old-token").unwrap();
        let paths = Paths::for_base(dir.path().join("cica"));
        super::prepare_bedrock_home(&paths, &home).unwrap();
        super::prepare_bedrock_home(&paths, &home).unwrap();
        assert_eq!(
            std::fs::read_link(paths.claude_home.join(".aws")).unwrap(),
            home.join(".aws")
        );
        std::fs::write(&cache, "refreshed-token").unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.claude_home.join(".aws/sso/cache/token.json")).unwrap(),
            "refreshed-token"
        );
    }

    const ANTHROPIC_CREDENTIAL_VARS: [&str; 4] = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ];

    #[test]
    fn bedrock_names_the_region_and_strips_every_anthropic_credential() {
        // An api_key is present on purpose: a deployment mid-migration still
        // carries one, and it must not reach the child.
        let env = applied(&ClaudeConfig {
            api_key: Some("sk-ant-api03-still-in-the-secret".into()),
            use_bedrock: true,
            bedrock_region: Some("eu-central-1".into()),
            ..Default::default()
        });

        assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK"), Some(&Some("1".into())));
        assert_eq!(env.get("AWS_REGION"), Some(&Some("eu-central-1".into())));
        for var in ANTHROPIC_CREDENTIAL_VARS {
            assert_eq!(
                env.get(var),
                Some(&None),
                "{var} must be removed, not passed through"
            );
        }
        assert!(!env.contains_key("CLAUDE_CODE_USE_VERTEX"));
    }

    #[test]
    fn bedrock_without_a_region_defers_to_the_aws_environment() {
        let env = applied(&ClaudeConfig {
            use_bedrock: true,
            bedrock_region: None,
            ..Default::default()
        });
        assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK"), Some(&Some("1".into())));
        assert!(
            !env.contains_key("AWS_REGION"),
            "an unset region must leave AWS_REGION to the environment, not blank it"
        );
    }

    #[test]
    fn an_empty_region_is_treated_as_unset() {
        let env = applied(&ClaudeConfig {
            use_bedrock: true,
            bedrock_region: Some(String::new()),
            ..Default::default()
        });
        assert!(!env.contains_key("AWS_REGION"));
    }

    #[test]
    fn bedrock_takes_precedence_over_vertex() {
        let env = applied(&ClaudeConfig {
            use_bedrock: true,
            use_vertex: true,
            vertex_project_id: Some("some-project".into()),
            ..Default::default()
        });
        assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK"), Some(&Some("1".into())));
        assert!(!env.contains_key("CLAUDE_CODE_USE_VERTEX"));
        assert!(!env.contains_key("ANTHROPIC_VERTEX_PROJECT_ID"));
    }

    #[test]
    fn an_api_key_still_reaches_claude_code_when_bedrock_is_off() {
        let env = applied(&ClaudeConfig {
            api_key: Some("sk-ant-api03-real".into()),
            ..Default::default()
        });
        assert_eq!(
            env.get("ANTHROPIC_API_KEY"),
            Some(&Some("sk-ant-api03-real".into()))
        );
        assert!(!env.contains_key("CLAUDE_CODE_USE_BEDROCK"));
    }

    #[test]
    fn an_oauth_token_still_reaches_claude_code_when_bedrock_is_off() {
        let env = applied(&ClaudeConfig {
            api_key: Some("sk-ant-oat01-token".into()),
            ..Default::default()
        });
        assert_eq!(
            env.get("CLAUDE_CODE_OAUTH_TOKEN"),
            Some(&Some("sk-ant-oat01-token".into()))
        );
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn vertex_credentials_resolve_from_config_directory() {
        let mut paths = crate::config::Paths::for_base(std::path::PathBuf::from("/worker"));
        paths.config_file = std::path::PathBuf::from("/router/config.toml");
        assert_eq!(
            config_relative_path(&paths, "credentials.json"),
            std::path::PathBuf::from("/router/credentials.json")
        );
    }

    const RESULT_ENVELOPE: &str = r#"{"type":"result","subtype":"success","result":"ok",
        "session_id":"s-1","duration_ms":5840,"total_cost_usd":0.0417,
        "modelUsage":{"claude-opus-4-6":{"inputTokens":12,"outputTokens":3}}}"#;

    #[test]
    fn parses_the_served_model_from_the_result_envelope() {
        let parsed: ClaudeResponse = serde_json::from_str(RESULT_ENVELOPE).unwrap();
        assert_eq!(
            served_models(&parsed.model_usage).as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn served_models_lists_every_model_a_turn_billed() {
        let parsed: ClaudeResponse = serde_json::from_str(
            r#"{"type":"result","modelUsage":{"claude-opus-4-6":{},"claude-haiku-4-5":{}}}"#,
        )
        .unwrap();
        assert_eq!(
            served_models(&parsed.model_usage).as_deref(),
            Some("claude-haiku-4-5, claude-opus-4-6")
        );
    }

    #[test]
    fn a_response_without_model_usage_still_parses() {
        let parsed: ClaudeResponse =
            serde_json::from_str(r#"{"type":"result","result":"ok","session_id":"s-1"}"#).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("ok"));
        assert!(served_models(&parsed.model_usage).is_none());
    }

    #[test]
    fn an_empty_model_usage_map_reports_nothing() {
        let parsed: ClaudeResponse =
            serde_json::from_str(r#"{"type":"result","modelUsage":{}}"#).unwrap();
        assert!(served_models(&parsed.model_usage).is_none());
    }

    #[test]
    fn detects_a_missing_conversation() {
        assert!(is_missing_conversation(
            "No conversation found with session ID: b1623d31-e974-4d04-a3ea-36493ce262f3"
        ));
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert!(is_missing_conversation(
            "no conversation found with session id: abc"
        ));
    }

    #[test]
    fn leaves_unrelated_failures_alone() {
        for stderr in [
            "Invalid API key",
            "rate limit exceeded",
            "session ID is malformed",
            "Error: connection reset by peer",
            "",
        ] {
            assert!(
                !is_missing_conversation(stderr),
                "should not match: {stderr}"
            );
        }
    }
}
