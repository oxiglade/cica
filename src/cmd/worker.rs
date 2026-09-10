//! `cica worker --turn <id>`: run exactly one turn, then exit.
//!
//! Reads the `TurnJob` from the state store, runs it through the same
//! `HydratingProvider` the in-process path uses, and writes the `TurnResult`
//! back to the store. Exits non-zero (without a result) on any failure.

use anyhow::{Result, anyhow};

use std::path::PathBuf;

use crate::config::{Config, Paths};
use crate::sandbox::LocalProcessProvider;
use crate::sandbox::state::default_store;
use crate::sandbox::warm::WarmHydratingProvider;
use crate::sandbox::worker::{Timing, WorkerSpec, run_worker_loop, run_worker_turn};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    turn_id: Option<&str>,
    session: Option<&str>,
    worker_id: Option<&str>,
    idle_secs: Option<u64>,
    turn_timeout_secs: Option<u64>,
    policy_hash: Option<&str>,
    home: Option<PathBuf>,
    deps: Option<PathBuf>,
    skills: Option<PathBuf>,
    config_file: Option<PathBuf>,
) -> Result<()> {
    let router = crate::config::paths()?;
    let mut router = router;
    if let Some(path) = config_file {
        router.config_file = path;
    }
    if let Some(path) = deps {
        router.deps_dir = path.clone();
        router.bun_dir = path.join("bun");
        router.java_dir = path.join("java");
        router.signal_cli_dir = path.join("signal-cli");
        router.claude_code_dir = path.join("claude-code");
        router.cursor_cli_dir = path.join("cursor-cli");
    }
    if let Some(path) = skills {
        router.skills_dir = path;
    }
    let paths = home.map_or_else(|| router.clone(), |base| Paths::for_worker(base, &router));
    let config = Config::load_from(&paths.config_file)?;

    let store = default_store(&config, &paths)?
        .ok_or_else(|| anyhow!("`cica worker` requires [deployment].store to be configured"))?;

    if let Some(turn_id) = turn_id {
        let engine = crate::sandbox::hydrating::HydratingProvider::new(
            LocalProcessProvider::for_worker(config.clone(), paths.clone()),
            store.clone(),
            paths.claude_home.clone(),
            paths.cursor_home.clone(),
            paths.base.clone(),
        );
        return run_worker_turn(store.as_ref(), &engine, turn_id).await;
    }
    let session = session.ok_or_else(|| anyhow!("`cica worker` requires --session or --turn"))?;
    let worker_id =
        worker_id.ok_or_else(|| anyhow!("`cica worker --session` requires --worker-id"))?;
    let idle = idle_secs.ok_or_else(|| anyhow!("`cica worker --session` requires --idle-secs"))?;
    let turn_timeout = turn_timeout_secs
        .ok_or_else(|| anyhow!("`cica worker --session` requires --turn-timeout-secs"))?;
    let policy_hash =
        policy_hash.ok_or_else(|| anyhow!("`cica worker --session` requires --policy-hash"))?;
    let timing = Timing {
        idle: std::time::Duration::from_secs(idle),
        turn_timeout: std::time::Duration::from_secs(turn_timeout),
        max_age: std::time::Duration::from_secs(config.deployment.worker_max_age_secs),
        ..Default::default()
    };
    let spec = WorkerSpec {
        session: session.into(),
        worker_id: worker_id.into(),
        launch_token: String::new(),
        idle: timing.idle,
        turn_timeout: timing.turn_timeout,
        start_timeout: timing.start_timeout,
        policy_hash: policy_hash.into(),
    };
    let engine = WarmHydratingProvider::new(
        LocalProcessProvider::for_worker(config.clone(), paths.clone()),
        store.clone(),
        paths.claude_home.clone(),
        paths.cursor_home.clone(),
        paths.base.clone(),
        Some((session.into(), worker_id.into())),
    );
    run_worker_loop(store, &engine, spec, timing).await
}
