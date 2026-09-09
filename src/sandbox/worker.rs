//! Worker dispatch: run a turn in a one-shot `cica worker` child process,
//! exchanging the job and result through the `StateStore` keyed by a turn id.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::{Instant, sleep};
use tracing::warn;
use uuid::Uuid;

use crate::sandbox::state::StateStore;
use crate::sandbox::{
    PROTOCOL_VERSION, SandboxProvider, TurnEnvelope, TurnJob, TurnOutcome, TurnResult,
};

fn job_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/job")
}

fn result_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/result")
}

/// Attachments are keyed by path, so one upload serves every turn that names it.
fn attachment_key(path: &str) -> String {
    format!("attachments/{path}")
}

/// Per turn, unlike inbound attachments: the agent picks these names, so two
/// turns can pick the same one.
fn produced_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/out")
}

const PRESERVATION_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_PRODUCED_BYTES: u64 = 100 * 1024 * 1024;

fn scratch_dir(turn_id: &str, kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "cica-turn-{turn_id}-{kind}-{}",
        uuid::Uuid::new_v4()
    ))
}

async fn push_attachments(store: &dyn StateStore, base: &std::path::Path, job: &TurnJob) {
    for relative in &job.attachments {
        let src = base.join(relative);
        if !src.is_file() {
            warn!("attachment {relative} not found at {src:?}; the worker will not see it");
            continue;
        }
        let Some(file_name) = src.file_name() else {
            warn!("attachment {relative} has no file name; the worker will not see it");
            continue;
        };
        let staging = std::env::temp_dir().join(format!("cica-attach-{}", Uuid::new_v4()));
        let copied = std::fs::create_dir_all(&staging)
            .and_then(|_| std::fs::copy(&src, staging.join(file_name)).map(|_| ()));
        if let Err(e) = copied {
            warn!("failed to stage attachment {relative}: {e}");
        } else if let Err(e) = store.push(&staging, &attachment_key(relative)).await {
            warn!("failed to push attachment {relative} to the store: {e}");
        }
        let _ = std::fs::remove_dir_all(&staging);
    }
}

async fn push_produced_files(store: &dyn StateStore, turn_id: &str, result: &mut TurnResult) {
    let markers = crate::sandbox::attachment_markers(&result.response);
    if markers.is_empty() {
        return;
    }

    let staging = std::env::temp_dir().join(format!("cica-produced-{}", Uuid::new_v4()));
    if let Err(e) = std::fs::create_dir_all(&staging) {
        warn!("failed to stage produced files: {e}");
        return;
    }

    let mut names = Vec::new();
    for marker in markers {
        let src = std::path::Path::new(&marker);
        let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if names.iter().any(|n| n == name) {
            warn!("two produced files share the name {name}; keeping the first");
            continue;
        }
        match std::fs::metadata(src) {
            Ok(meta) if !meta.is_file() => continue,
            Ok(meta) if meta.len() > MAX_PRODUCED_BYTES => {
                warn!(
                    "produced file {name} is {} bytes, over the {MAX_PRODUCED_BYTES} limit; not sending it",
                    meta.len()
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("produced file {marker} is not readable: {e}");
                continue;
            }
        }
        if let Err(e) = std::fs::copy(src, staging.join(name)) {
            warn!("failed to stage produced file {name}: {e}");
            continue;
        }
        names.push(name.to_string());
    }

    if !names.is_empty() {
        match store.push(&staging, &produced_key(turn_id)).await {
            Ok(()) => result.produced_files = names,
            Err(e) => warn!("failed to push produced files to the store: {e}"),
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
}

async fn pull_produced_files(
    store: &dyn StateStore,
    turn_id: &str,
    dest_root: &std::path::Path,
    result: &mut TurnResult,
) {
    if result.produced_files.is_empty() {
        return;
    }
    let dest = dest_root.join(turn_id);
    if let Err(e) = std::fs::create_dir_all(&dest) {
        warn!("failed to create {dest:?} for produced files: {e}");
        return;
    }
    match store.pull(&produced_key(turn_id), &dest).await {
        Ok(true) => result.response = rewrite_markers(&result.response, &dest),
        Ok(false) => warn!("worker reported produced files but the store had none"),
        Err(e) => warn!("failed to pull produced files: {e}"),
    }
}

fn rewrite_markers(response: &str, dir: &std::path::Path) -> String {
    let mut out = response.to_string();
    for marker in crate::sandbox::attachment_markers(response) {
        let Some(name) = std::path::Path::new(&marker).file_name() else {
            continue;
        };
        let local = dir.join(name);
        if local.is_file() {
            out = out.replace(
                &format!("[attachment:{marker}]"),
                &format!("[attachment:{}]", local.display()),
            );
        }
    }
    out
}

async fn pull_job(store: &dyn StateStore, turn_id: &str) -> Result<TurnJob> {
    let dir = scratch_dir(turn_id, "job-in");
    let found = store.pull(&job_key(turn_id), &dir).await?;
    let result = if found {
        let bytes = std::fs::read(dir.join("job.json")).context("reading job.json")?;
        serde_json::from_slice(&bytes).context("deserializing TurnJob")
    } else {
        Err(anyhow::anyhow!("no job found for turn {turn_id}"))
    };
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
async fn push_job(store: &dyn StateStore, turn_id: &str, job: &TurnJob) -> Result<()> {
    let dir = scratch_dir(turn_id, "job");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("job.json"), serde_json::to_vec(job)?)?;
    store.push(&dir, &job_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

async fn push_result(store: &dyn StateStore, envelope: &TurnEnvelope) -> Result<()> {
    store
        .put_record(
            &result_key(&envelope.turn_id),
            &serde_json::to_vec(envelope)?,
        )
        .await
}

/// `None` if the worker never wrote a result.
async fn pull_result(store: &dyn StateStore, turn_id: &str) -> Result<Option<TurnEnvelope>> {
    store
        .get_record(&result_key(turn_id))
        .await?
        .map(|bytes| serde_json::from_slice(&bytes).context("deserializing TurnEnvelope"))
        .transpose()
}

async fn turn_outcome(
    store: &dyn StateStore,
    turn_id: &str,
    result: Result<TurnResult>,
) -> TurnOutcome {
    match result {
        Ok(mut result) => {
            push_produced_files(store, turn_id, &mut result).await;
            TurnOutcome::Result(result)
        }
        Err(error) => TurnOutcome::Error(error.to_string()),
    }
}

pub async fn run_worker_turn(
    store: &dyn StateStore,
    engine: &dyn crate::sandbox::SandboxProvider,
    turn_id: &str,
) -> Result<()> {
    static WORKER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let job = pull_job(store, turn_id).await?;
    let affinity_id = job.affinity.id();
    let outcome = turn_outcome(store, turn_id, engine.run_turn(job).await).await;
    push_result(
        store,
        &TurnEnvelope {
            protocol_version: PROTOCOL_VERSION,
            affinity_id,
            turn_id: turn_id.to_string(),
            worker_id: WORKER_ID.get_or_init(|| Uuid::new_v4().to_string()).clone(),
            outcome,
        },
    )
    .await?;
    Ok(())
}

/// Configuration passed to a persistent worker process.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub session: String,
    pub worker_id: String,
    pub launch_token: String,
    pub idle: Duration,
    pub turn_timeout: Duration,
    pub start_timeout: Duration,
    pub policy_hash: String,
}

impl WorkerSpec {
    /// Returns the stable worker command-line contract.
    pub fn args(&self) -> Vec<String> {
        vec![
            "worker".into(),
            "--session".into(),
            self.session.clone(),
            "--worker-id".into(),
            self.worker_id.clone(),
            "--idle-secs".into(),
            self.idle.as_secs().to_string(),
            "--turn-timeout-secs".into(),
            self.turn_timeout.as_secs().to_string(),
            "--policy-hash".into(),
            self.policy_hash.clone(),
        ]
    }
}

/// Platform used to create a worker handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LauncherKind {
    Subprocess,
    Docker,
    Fargate,
}

/// Serializable identity of a worker on its launch platform.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Handle {
    pub kind: LauncherKind,
    pub id: String,
}

/// Observed worker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Status {
    Running,
    Stopped,
    NotFound,
    Unknown,
}

/// Result of waiting for a worker to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Terminated,
    NotFound,
    Unknown,
}

/// Creates, observes, reconciles, and stops persistent workers.
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Starts a worker and returns only after the platform reports it running.
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle>;
    /// Reads the current platform state for a worker handle.
    #[allow(dead_code)]
    async fn status(&self, handle: &Handle) -> Result<Status>;
    /// Requests termination and waits no longer than `deadline`.
    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome>;
    /// Finds a worker previously started with the spec's launch token.
    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum OwnerPhase {
    Launching,
    Running,
}

/// Router-written identity and launch state for one affinity worker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnerRecord {
    #[serde(default)]
    pub protocol_version: u32,
    pub phase: OwnerPhase,
    pub worker_id: String,
    pub launch_token: String,
    pub handle: Option<Handle>,
    pub launched_at_unix: u64,
    #[serde(default)]
    pub router_protocol_version: u32,
    pub policy_hash: String,
    pub affinity: crate::sandbox::Affinity,
}

/// Router-written pointer to the only assigned turn for an affinity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InboxRecord {
    pub protocol_version: u32,
    pub turn_id: String,
    pub worker_id: String,
    pub enqueued_at_unix: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum WorkerPhase {
    Booting,
    Ready,
    Running,
    Draining,
}

/// Worker-written liveness and current-turn state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeartbeatRecord {
    pub seq: u64,
    pub phase: WorkerPhase,
    pub current_turn: Option<String>,
    pub last_turn: Option<String>,
    pub protocol_version: u32,
    pub policy_hash: String,
}

#[derive(Debug, Clone)]
pub struct Timing {
    pub inbox_poll: Duration,
    pub heartbeat: Duration,
    pub stale_after: Duration,
    pub liveness_check: Duration,
    pub start_timeout: Duration,
    pub idle: Duration,
    pub turn_timeout: Duration,
    pub max_age: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            inbox_poll: Duration::from_secs(1),
            heartbeat: Duration::from_secs(10),
            stale_after: Duration::from_secs(30),
            liveness_check: Duration::from_secs(5),
            start_timeout: Duration::from_secs(180),
            idle: Duration::from_secs(600),
            turn_timeout: Duration::from_secs(900),
            max_age: Duration::from_secs(86_400),
        }
    }
}

#[derive(Clone)]
struct CachedOwner {
    record: OwnerRecord,
    last_dispatch: Instant,
}

struct SeenHeartbeat {
    seq: u64,
    seen_at: Instant,
}

pub struct LaunchedWorkerProvider {
    store: Arc<dyn StateStore>,
    launcher: Box<dyn Launcher>,
    base: PathBuf,
    timing: Timing,
    policy_hash: String,
    worker_cap: usize,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    owners: tokio::sync::Mutex<HashMap<String, CachedOwner>>,
    seen: Mutex<HashMap<String, SeenHeartbeat>>,
}

impl LaunchedWorkerProvider {
    pub fn new(
        store: Arc<dyn StateStore>,
        launcher: Box<dyn Launcher>,
        base: PathBuf,
        timing: Timing,
        policy_hash: String,
        worker_cap: usize,
    ) -> Self {
        Self {
            store,
            launcher,
            base,
            timing,
            policy_hash,
            worker_cap,
            locks: Mutex::new(HashMap::new()),
            owners: tokio::sync::Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn produced_dir(&self) -> PathBuf {
        crate::sandbox::outbound_dir(&self.base)
    }

    fn owner_key(id: &str) -> String {
        format!("sessions/{id}/owner")
    }
    fn inbox_key(id: &str) -> String {
        format!("sessions/{id}/inbox")
    }
    fn heartbeat_key(id: &str, worker: &str) -> String {
        format!("sessions/{id}/workers/{worker}")
    }

    fn spec(&self, affinity_id: &str, worker_id: String, launch_token: String) -> WorkerSpec {
        WorkerSpec {
            session: affinity_id.into(),
            worker_id,
            launch_token,
            idle: self.timing.idle,
            turn_timeout: self.timing.turn_timeout,
            start_timeout: self.timing.start_timeout,
            policy_hash: self.policy_hash.clone(),
        }
    }

    async fn read_record<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.store
            .get_record(key)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    async fn liveness(&self, affinity_id: &str, owner: &OwnerRecord) -> Result<Liveness> {
        let heartbeat_key = Self::heartbeat_key(affinity_id, &owner.worker_id);
        let read = || self.read_record::<HeartbeatRecord>(&heartbeat_key);
        let first = read().await?;
        if let Some(heartbeat) = first {
            if heartbeat.protocol_version != PROTOCOL_VERSION
                || heartbeat.policy_hash != self.policy_hash
                || heartbeat.phase == WorkerPhase::Draining
            {
                return Ok(Liveness::Gone);
            }
            let fresh = {
                let mut seen = self.seen.lock().unwrap();
                let entry = seen
                    .entry(owner.worker_id.clone())
                    .or_insert(SeenHeartbeat {
                        seq: heartbeat.seq,
                        seen_at: Instant::now(),
                    });
                if entry.seq != heartbeat.seq {
                    entry.seq = heartbeat.seq;
                    entry.seen_at = Instant::now();
                }
                entry.seen_at.elapsed() <= self.timing.stale_after
            };
            if fresh {
                return Ok(Liveness::Live);
            }
            sleep(Duration::from_millis(100)).await;
            return Ok(match read().await? {
                Some(next) if next.seq != heartbeat.seq => Liveness::Live,
                _ => Liveness::Gone,
            });
        }
        if owner.phase == OwnerPhase::Launching
            && unix_now().saturating_sub(owner.launched_at_unix)
                < self.timing.start_timeout.as_secs()
        {
            return Ok(Liveness::Booting);
        }
        sleep(Duration::from_millis(100)).await;
        Ok(if read().await?.is_some() {
            Liveness::Live
        } else {
            Liveness::Gone
        })
    }

    async fn launch_worker(
        &self,
        affinity: &crate::sandbox::Affinity,
        affinity_id: &str,
    ) -> Result<OwnerRecord> {
        let worker_id = Uuid::new_v4().to_string();
        let launch_token = Uuid::new_v4().to_string();
        let spec = self.spec(affinity_id, worker_id.clone(), launch_token.clone());
        let mut owner = OwnerRecord {
            protocol_version: PROTOCOL_VERSION,
            phase: OwnerPhase::Launching,
            worker_id,
            launch_token,
            handle: None,
            launched_at_unix: unix_now(),
            router_protocol_version: PROTOCOL_VERSION,
            policy_hash: self.policy_hash.clone(),
            affinity: affinity.clone(),
        };
        self.store
            .put_record(&Self::owner_key(affinity_id), &serde_json::to_vec(&owner)?)
            .await?;
        let handle = self.launcher.start(&spec).await?;
        owner.phase = OwnerPhase::Running;
        owner.handle = Some(handle);
        self.store
            .put_record(&Self::owner_key(affinity_id), &serde_json::to_vec(&owner)?)
            .await?;
        Ok(owner)
    }

    async fn ensure_worker(&self, affinity: &crate::sandbox::Affinity) -> Result<OwnerRecord> {
        let id = affinity.id();
        let cached = self.owners.lock().await.get(&id).cloned().map(|c| c.record);
        let mut owner = match cached {
            Some(owner) => Some(owner),
            None => self.read_record(&Self::owner_key(&id)).await?,
        };
        if let Some(current) = owner.as_mut() {
            if current.protocol_version != PROTOCOL_VERSION
                || current.router_protocol_version != PROTOCOL_VERSION
                || current.policy_hash != self.policy_hash
                || current.affinity != *affinity
            {
                if let Some(handle) = &current.handle {
                    match self
                        .launcher
                        .stop_and_wait(handle, Duration::from_secs(30))
                        .await?
                    {
                        StopOutcome::Terminated | StopOutcome::NotFound => owner = None,
                        StopOutcome::Unknown => {
                            anyhow::bail!("worker state unknown for session {id}")
                        }
                    }
                } else {
                    owner = None;
                }
            } else if current.phase == OwnerPhase::Launching {
                let spec = self.spec(&id, current.worker_id.clone(), current.launch_token.clone());
                if let Some(handle) = self.launcher.reconcile(&spec).await? {
                    current.handle = Some(handle);
                    current.phase = OwnerPhase::Running;
                    self.store
                        .put_record(&Self::owner_key(&id), &serde_json::to_vec(current)?)
                        .await?;
                }
            }
        }
        if let Some(current) = &owner {
            match self.liveness(&id, current).await {
                Err(_) => anyhow::bail!("worker state unknown for session {id}"),
                Ok(Liveness::Live | Liveness::Booting) => {}
                Ok(Liveness::Gone) => {
                    if let Some(handle) = &current.handle {
                        match self
                            .launcher
                            .stop_and_wait(handle, Duration::from_secs(30))
                            .await?
                        {
                            StopOutcome::Terminated | StopOutcome::NotFound => owner = None,
                            StopOutcome::Unknown => {
                                anyhow::bail!("worker state unknown for session {id}")
                            }
                        }
                    } else {
                        owner = None;
                    }
                }
            }
        }
        if owner.is_none() {
            let mut owners = self.owners.lock().await;
            owners.remove(&id);
            if owners.len() >= self.worker_cap {
                let candidates = owners
                    .iter()
                    .map(|(id, cached)| (id.clone(), cached.clone()))
                    .collect::<Vec<_>>();
                let mut idle = Vec::new();
                for (candidate_id, cached) in candidates {
                    let key = Self::heartbeat_key(&candidate_id, &cached.record.worker_id);
                    if let Some(heartbeat) = self.read_record::<HeartbeatRecord>(&key).await?
                        && heartbeat.phase == WorkerPhase::Ready
                        && heartbeat.current_turn.is_none()
                    {
                        idle.push((candidate_id, cached));
                    }
                }
                let victim = idle
                    .into_iter()
                    .min_by_key(|(_, cached)| cached.last_dispatch)
                    .map(|(id, cached)| (id, cached.record));
                let Some((victim_id, victim)) = victim else {
                    anyhow::bail!("all workers busy")
                };
                let Some(handle) = victim.handle else {
                    anyhow::bail!("all workers busy")
                };
                match self
                    .launcher
                    .stop_and_wait(&handle, Duration::from_secs(30))
                    .await?
                {
                    StopOutcome::Terminated | StopOutcome::NotFound => {
                        owners.remove(&victim_id);
                    }
                    StopOutcome::Unknown => {
                        anyhow::bail!("worker state unknown for session {victim_id}")
                    }
                }
            }
            drop(owners);
            owner = Some(self.launch_worker(affinity, &id).await?);
        }
        let owner = owner.unwrap();
        self.owners.lock().await.insert(
            id,
            CachedOwner {
                record: owner.clone(),
                last_dispatch: Instant::now(),
            },
        );
        Ok(owner)
    }
}

enum Liveness {
    Live,
    Booting,
    Gone,
}

struct CancelGuard {
    store: Arc<dyn StateStore>,
    turn_id: String,
    armed: bool,
}
impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            let store = self.store.clone();
            let key = format!("turns/{}/cancel", self.turn_id);
            tokio::spawn(async move {
                if let Err(error) = store.put_record(&key, b"{}").await {
                    warn!(
                        "failed to publish turn cancellation; worker watcher will also observe the inbox change: {error}"
                    );
                }
            });
        }
    }
}

#[async_trait]
impl SandboxProvider for LaunchedWorkerProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let affinity_id = job.affinity.id();
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(affinity_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let owner = self.ensure_worker(&job.affinity).await?;
        let turn_id = Uuid::new_v4().to_string();
        let mut cancel = CancelGuard {
            store: self.store.clone(),
            turn_id: turn_id.clone(),
            armed: true,
        };
        push_attachments(self.store.as_ref(), &self.base, &job).await;
        self.store
            .put_record(&job_key(&turn_id), &serde_json::to_vec(&job)?)
            .await?;
        let inbox = InboxRecord {
            protocol_version: PROTOCOL_VERSION,
            turn_id: turn_id.clone(),
            worker_id: owner.worker_id.clone(),
            enqueued_at_unix: unix_now(),
        };
        self.store
            .put_record(&Self::inbox_key(&affinity_id), &serde_json::to_vec(&inbox)?)
            .await?;
        let deadline = Instant::now() + self.timing.turn_timeout + Duration::from_secs(60);
        let mut liveness = Instant::now() + self.timing.liveness_check;
        let envelope = loop {
            match pull_result(self.store.as_ref(), &turn_id).await {
                Ok(Some(envelope)) => break envelope,
                Ok(None) => {}
                Err(error) => warn!(
                    "failed to poll turn result; retrying while the worker may still finish: {error}"
                ),
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "no result after {}s",
                    (self.timing.turn_timeout + Duration::from_secs(60)).as_secs()
                );
            }
            if Instant::now() >= liveness {
                match self.liveness(&affinity_id, &owner).await {
                    Ok(Liveness::Gone) => anyhow::bail!("worker vanished during turn"),
                    Ok(_) => {}
                    Err(error) => {
                        warn!("failed to check worker liveness; state remains unknown: {error}")
                    }
                }
                liveness = Instant::now() + self.timing.liveness_check;
            }
            sleep(self.timing.inbox_poll).await;
        };
        if envelope.turn_id != turn_id
            || envelope.affinity_id != affinity_id
            || envelope.worker_id != owner.worker_id
            || envelope.protocol_version != PROTOCOL_VERSION
        {
            anyhow::bail!("worker result identity mismatch")
        }
        cancel.armed = false;
        let outcome = match envelope.outcome {
            TurnOutcome::Result(mut result) => {
                pull_produced_files(
                    self.store.as_ref(),
                    &turn_id,
                    &self.produced_dir(),
                    &mut result,
                )
                .await;
                Ok(result)
            }
            TurnOutcome::Error(error) => Err(anyhow::anyhow!(error)),
        };
        if let Err(error) = self
            .store
            .delete_record(&Self::inbox_key(&affinity_id))
            .await
        {
            warn!("failed to delete the session inbox; replay suppression keeps it inert: {error}");
        }
        if let Err(error) = self.store.delete(&format!("turns/{turn_id}")).await {
            warn!(
                "failed to delete completed turn {turn_id}; replay suppression keeps it inert: {error}"
            );
        }
        outcome
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn put_json<T: serde::Serialize>(store: &dyn StateStore, key: &str, value: &T) -> Result<()> {
    store.put_record(key, &serde_json::to_vec(value)?).await
}

async fn still_owner(store: &dyn StateStore, session: &str, worker_id: &str) -> Result<bool> {
    let Some(bytes) = store
        .get_record(&LaunchedWorkerProvider::owner_key(session))
        .await?
    else {
        return Ok(false);
    };
    let owner: OwnerRecord = serde_json::from_slice(&bytes)?;
    Ok(owner.worker_id == worker_id)
}

async fn watch_abort(
    store: &dyn StateStore,
    session: &str,
    turn_id: &str,
    worker_id: &str,
    poll: Duration,
) {
    loop {
        if matches!(
            store.get_record(&format!("turns/{turn_id}/cancel")).await,
            Ok(Some(_))
        ) {
            return;
        }
        if let Ok(Some(bytes)) = store
            .get_record(&LaunchedWorkerProvider::inbox_key(session))
            .await
            && serde_json::from_slice::<InboxRecord>(&bytes).map_or(true, |inbox| {
                inbox.turn_id != turn_id || inbox.worker_id != worker_id
            })
        {
            return;
        }
        sleep(poll).await;
    }
}

pub async fn run_worker_loop<P: SandboxProvider>(
    store: Arc<dyn StateStore>,
    engine: &crate::sandbox::warm::WarmHydratingProvider<P>,
    spec: WorkerSpec,
    timing: Timing,
) -> Result<()> {
    let heartbeat_key = LaunchedWorkerProvider::heartbeat_key(&spec.session, &spec.worker_id);
    let heartbeat = Arc::new(tokio::sync::Mutex::new(HeartbeatRecord {
        seq: 1,
        phase: WorkerPhase::Booting,
        current_turn: None,
        last_turn: None,
        protocol_version: PROTOCOL_VERSION,
        policy_hash: spec.policy_hash.clone(),
    }));
    put_json(store.as_ref(), &heartbeat_key, &*heartbeat.lock().await).await?;
    engine.warm_up().await;
    {
        let mut hb = heartbeat.lock().await;
        hb.phase = WorkerPhase::Ready;
        hb.seq += 1;
        put_json(store.as_ref(), &heartbeat_key, &*hb).await?;
    }
    let ticker_store = store.clone();
    let ticker_key = heartbeat_key.clone();
    let ticker_hb = heartbeat.clone();
    let heartbeat_every = timing.heartbeat;
    let ticker = tokio::spawn(async move {
        loop {
            sleep(heartbeat_every).await;
            let mut hb = ticker_hb.lock().await;
            hb.seq += 1;
            if let Err(error) = put_json(ticker_store.as_ref(), &ticker_key, &*hb).await {
                warn!("failed to write worker heartbeat; retrying next tick: {error}");
            }
        }
    });
    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut consumed = HashSet::new();
    let mut draining = false;
    loop {
        let age_expired = started.elapsed() >= timing.max_age;
        let idle_expired = last_activity.elapsed() >= timing.idle;
        if age_expired || idle_expired {
            if !draining {
                draining = true;
                let mut hb = heartbeat.lock().await;
                hb.phase = WorkerPhase::Draining;
                hb.seq += 1;
                if let Err(error) = put_json(store.as_ref(), &heartbeat_key, &*hb).await {
                    warn!(
                        "failed to announce worker drain; final inbox read still prevents a silent loss: {error}"
                    );
                }
            } else {
                break;
            }
        }
        let inbox = match store
            .get_record(&LaunchedWorkerProvider::inbox_key(&spec.session))
            .await
        {
            Ok(Some(bytes)) => match serde_json::from_slice::<InboxRecord>(&bytes) {
                Ok(value) => Some(value),
                Err(error) => {
                    warn!("failed to decode inbox; retrying next poll: {error}");
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                warn!("failed to poll worker inbox; retrying next poll: {error}");
                None
            }
        };
        let Some(inbox) = inbox.filter(|value| {
            value.worker_id == spec.worker_id && !consumed.contains(&value.turn_id)
        }) else {
            if draining {
                break;
            }
            let poll = if last_activity.elapsed() >= Duration::from_secs(60) {
                Duration::from_secs(5)
            } else {
                timing.inbox_poll
            };
            sleep(poll).await;
            continue;
        };
        consumed.insert(inbox.turn_id.clone());
        let job = match store.get_record(&job_key(&inbox.turn_id)).await {
            Ok(Some(bytes)) => match serde_json::from_slice::<TurnJob>(&bytes) {
                Ok(job) => job,
                Err(error) => {
                    warn!(
                        "failed to decode turn job; leaving it consumed to prevent replay: {error}"
                    );
                    continue;
                }
            },
            Ok(None) => {
                warn!("inbox named a turn without a job; leaving it consumed to prevent replay");
                continue;
            }
            Err(error) => {
                warn!("failed to read turn job; retrying store on the next loop: {error}");
                consumed.remove(&inbox.turn_id);
                continue;
            }
        };
        {
            let mut hb = heartbeat.lock().await;
            hb.phase = WorkerPhase::Running;
            hb.current_turn = Some(inbox.turn_id.clone());
            hb.seq += 1;
            if let Err(error) = put_json(store.as_ref(), &heartbeat_key, &*hb).await {
                warn!(
                    "failed to acknowledge turn pickup; executing because the inbox assignment is authoritative: {error}"
                );
            }
        }
        let turn_id = inbox.turn_id.clone();
        let (outcome, abandoned) = {
            let run = engine.run_turn(job.clone());
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => (Some(turn_outcome(store.as_ref(), &turn_id, result).await), false),
                _ = sleep(timing.turn_timeout) => (Some(TurnOutcome::Error("turn timed out".into())), true),
                _ = watch_abort(store.as_ref(), &spec.session, &turn_id, &spec.worker_id, timing.inbox_poll) => (None, true),
            }
        };
        if abandoned {
            if tokio::time::timeout(
                PRESERVATION_TIMEOUT,
                engine.preserve_abandoned(&job, &turn_id),
            )
            .await
            .is_err()
            {
                warn!(%turn_id, "Abandoned transcript preservation timed out");
            }
            engine.abandon(&job);
        }
        let timed_out =
            matches!(&outcome, Some(TurnOutcome::Error(error)) if error == "turn timed out");
        if let Some(outcome) = outcome {
            match still_owner(store.as_ref(), &spec.session, &spec.worker_id).await {
                Ok(true) => {
                    let envelope = TurnEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        affinity_id: spec.session.clone(),
                        turn_id: turn_id.clone(),
                        worker_id: spec.worker_id.clone(),
                        outcome,
                    };
                    if let Err(error) = push_result(store.as_ref(), &envelope).await {
                        warn!(
                            "failed to write turn result; router will keep polling and liveness remains authoritative: {error}"
                        );
                    }
                }
                Ok(false) => engine.abandon(&job),
                Err(error) => {
                    engine.abandon(&job);
                    warn!(
                        "failed to verify worker ownership; skipping result to avoid a stale worker becoming canonical: {error}"
                    );
                }
            }
        }
        {
            let mut hb = heartbeat.lock().await;
            hb.phase = if draining {
                WorkerPhase::Draining
            } else {
                WorkerPhase::Ready
            };
            hb.current_turn = None;
            hb.last_turn = Some(turn_id);
            hb.seq += 1;
            if let Err(error) = put_json(store.as_ref(), &heartbeat_key, &*hb).await {
                warn!(
                    "failed to write turn completion heartbeat; retrying on the heartbeat tick: {error}"
                );
            }
        }
        last_activity = Instant::now();
        if timed_out {
            break;
        }
        if age_expired {
            draining = true;
        }
    }
    ticker.abort();
    if let Err(error) = store.delete_record(&heartbeat_key).await {
        warn!(
            "failed to delete heartbeat on clean exit; its unchanged sequence will become stale: {error}"
        );
    }
    Ok(())
}

/// Launcher that spawns `cica worker --turn <id>` as a local child process.
pub struct SubprocessLauncher {
    self_exe: PathBuf,
    router_paths: crate::config::Paths,
}

impl SubprocessLauncher {
    pub fn new(self_exe: PathBuf, router_paths: crate::config::Paths) -> Self {
        Self {
            self_exe,
            router_paths,
        }
    }

    fn worker_home(&self, worker_id: &str) -> PathBuf {
        self.router_paths
            .internal_dir
            .join("workers")
            .join(worker_id)
    }

    fn isolation_args(&self, home: &Path) -> Vec<String> {
        vec![
            "--home".into(),
            home.display().to_string(),
            "--deps".into(),
            self.router_paths.deps_dir.display().to_string(),
            "--skills".into(),
            self.router_paths.skills_dir.display().to_string(),
            "--config".into(),
            self.router_paths.config_file.display().to_string(),
        ]
    }
}

async fn process_start_time(pid: u32) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return Ok(None);
        }
        let info = unsafe { info.assume_init() };
        Ok(Some(format!(
            "{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        )))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let fields = stat
            .rsplit_once(')')
            .context("invalid /proc pid stat")?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        Ok(fields.get(19).map(|value| (*value).to_string()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .await
            .context("reading process start time")?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
}

fn read_pid_file(path: &Path) -> Result<(u32, String)> {
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("reading subprocess handle {}", path.display()))?;
    let (pid, start_time) = value
        .trim()
        .split_once(':')
        .context("invalid subprocess handle")?;
    Ok((
        pid.parse().context("invalid subprocess pid")?,
        start_time.into(),
    ))
}

fn signal_process(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

async fn subprocess_matches(path: &Path) -> Result<Option<u32>> {
    let (pid, expected) = match read_pid_file(path) {
        Ok(value) => value,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if signal_process(pid, 0).is_err() {
        return Ok(None);
    }
    Ok((process_start_time(pid).await?.as_deref() == Some(expected.as_str())).then_some(pid))
}

#[async_trait]
impl Launcher for SubprocessLauncher {
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
        let _ = spec.start_timeout;
        let home = self.worker_home(&spec.worker_id);
        std::fs::create_dir_all(&home)?;
        let mut args = spec.args();
        args.extend(self.isolation_args(&home));
        let mut child = Command::new(&self.self_exe)
            .args(args)
            .kill_on_drop(false)
            .spawn()
            .context("spawning cica worker")?;
        let pid = child.id().context("spawned worker has no pid")?;
        let start_time = process_start_time(pid)
            .await?
            .context("worker exited before its start time could be read")?;
        let pid_file = home.join(format!("launch.{}.pid", spec.launch_token));
        std::fs::write(&pid_file, format!("{pid}:{start_time}"))?;
        sleep(Duration::from_secs(2)).await;
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("worker exited during startup with status {status}");
        }
        Ok(Handle {
            kind: LauncherKind::Subprocess,
            id: pid_file.display().to_string(),
        })
    }

    async fn status(&self, handle: &Handle) -> Result<Status> {
        Ok(
            if subprocess_matches(Path::new(&handle.id)).await?.is_some() {
                Status::Running
            } else {
                Status::NotFound
            },
        )
    }

    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome> {
        let path = Path::new(&handle.id);
        let Some(pid) = subprocess_matches(path).await? else {
            return Ok(StopOutcome::NotFound);
        };
        if signal_process(pid, libc::SIGTERM).is_err() {
            return Ok(StopOutcome::NotFound);
        }
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if subprocess_matches(path).await?.is_none() {
                return Ok(StopOutcome::Terminated);
            }
            sleep(Duration::from_millis(50)).await;
        }
        let _ = signal_process(pid, libc::SIGKILL);
        sleep(Duration::from_millis(50)).await;
        Ok(if subprocess_matches(path).await?.is_none() {
            StopOutcome::Terminated
        } else {
            StopOutcome::Unknown
        })
    }

    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
        let path = self
            .worker_home(&spec.worker_id)
            .join(format!("launch.{}.pid", spec.launch_token));
        Ok(subprocess_matches(&path).await?.map(|_| Handle {
            kind: LauncherKind::Subprocess,
            id: path.display().to_string(),
        }))
    }
}

/// Launcher that runs `cica worker --turn <id>` inside a one-shot container.
///
/// Mounts the host config, published skills, and filesystem state-store into a
/// `/data/cica`-pinned container (the image sets `XDG_CONFIG_HOME=/data`).
/// `cursor-home`/`claude-home` stay container-local (fresh per turn).
pub struct DockerLauncher {
    image: String,
    config_file: PathBuf,
    skills_dir: Option<PathBuf>,
    state_store_dir: PathBuf,
    env: Vec<(String, String)>,
}

impl DockerLauncher {
    /// `skills_dir` is `None` when the state store carries the skills.
    pub fn new(
        image: String,
        config_file: PathBuf,
        skills_dir: Option<PathBuf>,
        state_store_dir: PathBuf,
    ) -> Self {
        Self {
            image,
            config_file,
            skills_dir,
            state_store_dir,
            env: Vec::new(),
        }
    }

    #[cfg(test)]
    fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// The `docker` argv (without the leading `docker`). Pure, for testing.
    fn worker_name(spec: &WorkerSpec) -> String {
        let slug = spec.session.chars().take(24).collect::<String>();
        format!("cica-{slug}-{}", spec.launch_token)
    }

    fn worker_run_args(&self, spec: &WorkerSpec) -> Vec<String> {
        let name = Self::worker_name(spec);
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--name".into(),
            name,
            "--label".into(),
            format!("cica.session={}", spec.session),
            "--label".into(),
            format!("cica.launch_token={}", spec.launch_token),
            "-e".into(),
            "CICA_STATE_PATH=/data/cica/internal/state-store".into(),
        ];
        for (key, value) in &self.env {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.extend([
            "-v".into(),
            format!("{}:/data/cica/config.toml:ro", self.config_file.display()),
        ]);
        if let Some(skills_dir) = &self.skills_dir {
            args.extend([
                "-v".into(),
                format!("{}:/data/cica/skills:ro", skills_dir.display()),
            ]);
        }
        args.extend([
            "-v".into(),
            format!(
                "{}:/data/cica/internal/state-store",
                self.state_store_dir.display()
            ),
            self.image.clone(),
        ]);
        args.extend(spec.args());
        args
    }

    async fn inspect_running(name: &str) -> Result<Option<bool>> {
        let output = Command::new("docker")
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .await
            .context("running docker inspect")?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim() == "true",
        ))
    }
}

#[async_trait]
impl Launcher for DockerLauncher {
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
        let name = Self::worker_name(spec);
        let output = Command::new("docker")
            .args(self.worker_run_args(spec))
            .output()
            .await
            .context("starting cica worker container")?;
        if !output.status.success() {
            anyhow::bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        sleep(Duration::from_secs(2)).await;
        if Self::inspect_running(&name).await? != Some(true) {
            anyhow::bail!("worker container {name} exited during startup");
        }
        Ok(Handle {
            kind: LauncherKind::Docker,
            id: name,
        })
    }

    async fn status(&self, handle: &Handle) -> Result<Status> {
        Ok(match Self::inspect_running(&handle.id).await? {
            Some(true) => Status::Running,
            Some(false) => Status::Stopped,
            None => Status::NotFound,
        })
    }

    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome> {
        if Self::inspect_running(&handle.id).await?.is_none() {
            return Ok(StopOutcome::NotFound);
        }
        let status = Command::new("docker")
            .args(["stop", "-t", &deadline.as_secs().to_string(), &handle.id])
            .status()
            .await
            .context("stopping cica worker container")?;
        if !status.success() && Self::inspect_running(&handle.id).await?.is_some() {
            return Ok(StopOutcome::Unknown);
        }
        Ok(match Self::inspect_running(&handle.id).await? {
            None | Some(false) => StopOutcome::Terminated,
            Some(true) => StopOutcome::Unknown,
        })
    }

    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
        let output = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=cica.launch_token={}", spec.launch_token),
            ])
            .output()
            .await
            .context("reconciling cica worker container")?;
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            return Ok(None);
        }
        let name = Self::worker_name(spec);
        Ok(Some(Handle {
            kind: LauncherKind::Docker,
            id: name,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AiBackend, Paths};
    use crate::sandbox::state::FilesystemStateStore;

    fn sample_job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
            attachments: Vec::new(),
        }
    }

    fn sample_result(response: &str) -> TurnResult {
        TurnResult {
            response: response.into(),
            backend_session_id: "s1".into(),
            cost_usd: None,
            duration_ms: None,
            produced_files: Vec::new(),
        }
    }

    fn worker_spec() -> WorkerSpec {
        WorkerSpec {
            session: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            worker_id: "worker-1".into(),
            launch_token: "token-1".into(),
            idle: Duration::from_secs(600),
            turn_timeout: Duration::from_secs(900),
            start_timeout: Duration::from_secs(180),
            policy_hash: "policy".into(),
        }
    }

    #[test]
    fn worker_spec_builds_worker_args() {
        assert_eq!(worker_spec().start_timeout, Duration::from_secs(180));
        assert_eq!(
            worker_spec().args(),
            [
                "worker",
                "--session",
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
                "--worker-id",
                "worker-1",
                "--idle-secs",
                "600",
                "--turn-timeout-secs",
                "900",
                "--policy-hash",
                "policy"
            ]
        );
    }

    #[tokio::test]
    async fn subprocess_worker_starts_reconciles_and_stops() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("worker.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::for_base(root.path().join("router"));
        let launcher = SubprocessLauncher::new(script, paths);
        let spec = worker_spec();

        let handle = launcher.start(&spec).await.unwrap();
        assert_eq!(launcher.status(&handle).await.unwrap(), Status::Running);
        assert_eq!(
            launcher.reconcile(&spec).await.unwrap(),
            Some(handle.clone())
        );
        assert_eq!(
            launcher
                .stop_and_wait(&handle, Duration::from_secs(1))
                .await
                .unwrap(),
            StopOutcome::Terminated
        );
        assert_eq!(launcher.status(&handle).await.unwrap(), Status::NotFound);
    }

    #[tokio::test]
    async fn subprocess_reconcile_rejects_changed_start_time() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("worker.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::for_base(root.path().join("router"));
        let launcher = SubprocessLauncher::new(script, paths);
        let spec = worker_spec();
        let handle = launcher.start(&spec).await.unwrap();
        let (pid, _) = read_pid_file(Path::new(&handle.id)).unwrap();
        std::fs::write(&handle.id, format!("{pid}:different start time")).unwrap();

        assert!(launcher.reconcile(&spec).await.unwrap().is_none());
        let _ = signal_process(pid, libc::SIGKILL);
    }

    #[test]
    fn docker_worker_run_args_carry_identity_and_contract() {
        let launcher = DockerLauncher::new(
            "image".into(),
            PathBuf::from("/config"),
            Some(PathBuf::from("/skills")),
            PathBuf::from("/state"),
        );
        let args = launcher.worker_run_args(&worker_spec());

        assert!(args.contains(&"cica.session=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into()));
        assert!(args.contains(&"cica.launch_token=token-1".into()));
        assert!(args.contains(&"cica-abcdefghijklmnopqrstuvwx-token-1".into()));
        let worker_args = worker_spec().args();
        assert_eq!(&args[args.len() - worker_args.len()..], worker_args);
    }

    #[tokio::test]
    async fn a_file_the_worker_wrote_reaches_the_router() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());

        let worker_dir = tempfile::tempdir().unwrap();
        let produced = worker_dir.path().join("cft_input.json");
        std::fs::write(&produced, br#"{"herd": 120}"#).unwrap();
        let mut result = sample_result(&format!(
            "[attachment:{}]\n\nHere is the CFT input.",
            produced.display()
        ));

        push_produced_files(&store, "t-out", &mut result).await;
        assert_eq!(result.produced_files, vec!["cft_input.json".to_string()]);

        std::fs::remove_dir_all(worker_dir.path()).unwrap();
        let router_root = tempfile::tempdir().unwrap();
        pull_produced_files(&store, "t-out", router_root.path(), &mut result).await;

        let landed = router_root.path().join("t-out").join("cft_input.json");
        assert!(landed.is_file(), "the file did not reach the router");
        assert_eq!(std::fs::read(&landed).unwrap(), br#"{"herd": 120}"#);
        assert!(
            result
                .response
                .contains(&format!("[attachment:{}]", landed.display())),
            "the marker still points at the worker path: {}",
            result.response
        );
    }

    #[tokio::test]
    async fn a_turn_with_no_marker_ships_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let mut result = sample_result("Just an answer, no files.");
        push_produced_files(&store, "t-none", &mut result).await;
        assert!(result.produced_files.is_empty());
    }

    #[tokio::test]
    async fn a_marker_for_a_missing_file_is_survivable() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let mut result = sample_result("[attachment:/nope/gone.json]\n\nText survives.");
        push_produced_files(&store, "t-missing", &mut result).await;
        assert!(result.produced_files.is_empty());
        assert!(result.response.contains("Text survives."));
    }

    #[tokio::test]
    async fn a_traversing_name_is_flattened() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let evil = dir.path().join("passwd");
        std::fs::write(&evil, b"x").unwrap();

        let mut result = sample_result(&format!("[attachment:{}]", evil.display()));
        push_produced_files(&store, "t-evil", &mut result).await;
        assert_eq!(result.produced_files, vec!["passwd".to_string()]);
        assert!(
            !result.produced_files.iter().any(|n| n.contains('/')),
            "a stored name kept a path separator"
        );
    }

    #[test]
    fn a_result_without_produced_files_still_deserializes() {
        let json =
            r#"{"response":"hi","backend_session_id":"s","cost_usd":null,"duration_ms":null}"#;
        let r: TurnResult = serde_json::from_str(json).unwrap();
        assert!(r.produced_files.is_empty());
    }

    #[tokio::test]
    async fn job_push_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        push_job(&store, "t1", &sample_job()).await.unwrap();
        let back = pull_job(&store, "t1").await.unwrap();
        assert_eq!(back.channel, "telegram");
        assert_eq!(back.prompt, "hi");
    }

    #[tokio::test]
    async fn pull_result_none_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        assert!(pull_result(&store, "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn result_push_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess".into(),
            cost_usd: None,
            duration_ms: None,
            produced_files: Vec::new(),
        };
        push_result(
            &store,
            &TurnEnvelope {
                protocol_version: PROTOCOL_VERSION,
                affinity_id: sample_job().affinity.id(),
                turn_id: "t2".into(),
                worker_id: "w1".into(),
                outcome: TurnOutcome::Result(result),
            },
        )
        .await
        .unwrap();
        let back = pull_result(&store, "t2").await.unwrap().unwrap();
        let TurnOutcome::Result(back) = back.outcome else {
            panic!("expected result")
        };
        assert_eq!(back.backend_session_id, "sess");
    }

    #[tokio::test]
    async fn run_worker_turn_reads_job_and_writes_result() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        push_job(&store, "tX", &sample_job()).await.unwrap();

        struct StubEngine;
        #[async_trait::async_trait]
        impl crate::sandbox::SandboxProvider for StubEngine {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                Ok(TurnResult {
                    response: "from-worker".into(),
                    backend_session_id: "sess-w".into(),
                    cost_usd: None,
                    duration_ms: None,
                    produced_files: Vec::new(),
                })
            }
        }

        run_worker_turn(&store, &StubEngine, "tX").await.unwrap();
        let result = pull_result(&store, "tX").await.unwrap().unwrap();
        let TurnOutcome::Result(result) = result.outcome else {
            panic!("expected result")
        };
        assert_eq!(result.response, "from-worker");
        assert_eq!(result.backend_session_id, "sess-w");
    }

    #[tokio::test]
    async fn subprocess_warm_worker_reuses_one_process() {
        use std::os::unix::fs::PermissionsExt;

        let Some(binary) = std::env::var_os("CICA_BIN") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let router = Paths::for_base(root.path().join("router"));
        std::fs::create_dir_all(&router.base).unwrap();
        let state = router.internal_dir.join("state-store");
        std::fs::write(
            &router.config_file,
            format!(
                "backend = \"cursor\"\n[deployment]\nstore = \"filesystem\"\nstate_path = {:?}\n",
                state.display().to_string()
            ),
        )
        .unwrap();
        let wrapper = root.path().join("cica-test-worker");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nCICA_FAKE_BACKEND=1 exec {:?} \"$@\"\n",
                PathBuf::from(binary).display().to_string()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Arc::new(FilesystemStateStore::new(state));
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(SubprocessLauncher::new(wrapper.clone(), router.clone())),
            router.base.clone(),
            Timing::default(),
            "subprocess-policy".into(),
            32,
        );
        let first = provider.run_turn(sample_job()).await.unwrap();
        let affinity = sample_job().affinity.id();
        let first_owner: OwnerRecord = serde_json::from_slice(
            &store
                .get_record(&LaunchedWorkerProvider::owner_key(&affinity))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let second = provider.run_turn(sample_job()).await.unwrap();
        let second_owner: OwnerRecord = serde_json::from_slice(
            &store
                .get_record(&LaunchedWorkerProvider::owner_key(&affinity))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(first.response.contains("fake-response: hi"));
        assert!(second.response.contains("fake-response: hi"));
        assert_eq!(first_owner.worker_id, second_owner.worker_id);
        let launcher = SubprocessLauncher::new(wrapper, router);
        assert_eq!(
            launcher
                .stop_and_wait(
                    second_owner.handle.as_ref().unwrap(),
                    Duration::from_secs(5)
                )
                .await
                .unwrap(),
            StopOutcome::Terminated
        );
    }

    #[tokio::test]
    async fn docker_flow_round_trips_with_fake_backend() {
        if std::env::var_os("CICA_DOCKER_IT").is_none() {
            return;
        }
        let store_root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));
        let cfg_dir = tempfile::tempdir().unwrap();
        let config_file = cfg_dir.path().join("config.toml");
        std::fs::write(
            &config_file,
            "backend = \"cursor\"\n[deployment]\nstore = \"filesystem\"\n",
        )
        .unwrap();
        let skills_dir = tempfile::tempdir().unwrap();

        let launcher = DockerLauncher::new(
            "cica-worker:latest".into(),
            config_file,
            Some(skills_dir.path().to_path_buf()),
            store_root.path().to_path_buf(),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            cfg_dir.path().join("base"),
            Timing::default(),
            "docker-policy".into(),
            32,
        );
        let first = provider.run_turn(sample_job()).await.unwrap();
        let second = provider.run_turn(sample_job()).await.unwrap();
        assert!(first.response.contains("fake-response: hi"));
        assert!(second.response.contains("fake-response: hi"));
        let affinity = sample_job().affinity.id();
        let owner: OwnerRecord = serde_json::from_slice(
            &store
                .get_record(&LaunchedWorkerProvider::owner_key(&affinity))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let output = Command::new("docker")
            .args([
                "ps",
                "-q",
                "--filter",
                &format!("label=cica.session={affinity}"),
            ])
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
        let launcher = DockerLauncher::new(
            "cica-worker:latest".into(),
            cfg_dir.path().join("config.toml"),
            None,
            store_root.path().to_path_buf(),
        );
        assert_eq!(
            launcher
                .stop_and_wait(owner.handle.as_ref().unwrap(), Duration::from_secs(10))
                .await
                .unwrap(),
            StopOutcome::Terminated
        );
    }
}

#[cfg(test)]
mod warm_protocol_tests {
    use super::*;
    use crate::config::AiBackend;
    use crate::sandbox::state::FilesystemStateStore;
    use crate::sandbox::warm::WarmHydratingProvider;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct PreservationProbe {
        inner: FilesystemStateStore,
        dropped: Arc<AtomicBool>,
        observed: tokio::sync::mpsc::UnboundedSender<(bool, PathBuf)>,
        stall: bool,
    }

    #[async_trait]
    impl StateStore for PreservationProbe {
        async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get_record(key).await
        }
        async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
            self.inner.put_record(key, bytes).await
        }
        async fn delete_record(&self, key: &str) -> Result<()> {
            self.inner.delete_record(key).await
        }
        async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
            self.inner.pull(key, dest).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key).await
        }
        async fn push(&self, src: &Path, key: &str) -> Result<()> {
            if key.starts_with("abandoned/") {
                self.observed
                    .send((self.dropped.load(Ordering::SeqCst), src.to_path_buf()))
                    .unwrap();
                if self.stall {
                    std::future::pending::<()>().await;
                }
            }
            self.inner.push(src, key).await
        }
    }

    struct AgentDropGuard(Arc<AtomicBool>);
    impl Drop for AgentDropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct GuardedAgent(Arc<AtomicBool>);
    #[async_trait]
    impl SandboxProvider for GuardedAgent {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            let _guard = AgentDropGuard(self.0.clone());
            std::future::pending().await
        }
    }

    async fn probe_preservation(stall: bool) {
        let root = tempfile::tempdir().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let (observed, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = Arc::new(PreservationProbe {
            inner: FilesystemStateStore::new(root.path().join("store")),
            dropped: dropped.clone(),
            observed,
            stall,
        });
        let transcript = root
            .path()
            .join("claude/.claude/projects/-work/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "partial").unwrap();
        let seed = root.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::write(seed.join("transcript.jsonl"), "partial").unwrap();
        store.push(&seed, "session/session").await.unwrap();
        let mut worker_timing = timing();
        worker_timing.turn_timeout = Duration::from_millis(50);
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            GuardedAgent(dropped),
            worker_timing,
        )
        .await;
        let mut turn = job();
        turn.resume_session = Some("session".into());
        turn.session_persistence = crate::sandbox::SessionPersistence::Resume;
        assign(store.as_ref(), &affinity, &worker, "probe").await;
        store
            .put_record(&job_key("probe"), &serde_json::to_vec(&turn).unwrap())
            .await
            .unwrap();
        let (dropped_before_push, scratch) =
            tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap();
        if stall {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
        }
        let finished = tokio::time::timeout(Duration::from_secs(1), task).await;
        assert!(finished.is_ok(), "preservation exceeded its deadline");
        finished.unwrap().unwrap().unwrap();
        assert!(
            dropped_before_push,
            "agent guard was still alive when archive push began"
        );
        assert!(!scratch.exists(), "preservation scratch survived");
        assert!(
            !transcript.exists(),
            "abandonment did not discard the local transcript"
        );
        assert!(
            store
                .get_record(&result_key("probe"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn worker_drops_agent_future_before_archive_push_begins() {
        probe_preservation(false).await;
    }

    #[tokio::test]
    async fn preservation_deadline_still_abandons_and_cleans_scratch() {
        probe_preservation(true).await;
    }

    struct CountingStore {
        inner: FilesystemStateStore,
        skill_pulls: AtomicUsize,
    }

    #[async_trait]
    impl StateStore for CountingStore {
        async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get_record(key).await
        }

        async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
            self.inner.put_record(key, bytes).await
        }

        async fn delete_record(&self, key: &str) -> Result<()> {
            self.inner.delete_record(key).await
        }

        async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
            if key == "skills" {
                self.skill_pulls.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.pull(key, dest).await
        }

        async fn push(&self, src: &Path, key: &str) -> Result<()> {
            self.inner.push(src, key).await
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key).await
        }
    }

    #[derive(Default)]
    struct FaultStore {
        records: Mutex<HashMap<String, Vec<u8>>>,
        fail_get: Mutex<HashSet<String>>,
        fail_put: Mutex<HashSet<String>>,
        puts: Mutex<Vec<(String, Vec<u8>)>>,
    }

    #[async_trait]
    impl StateStore for FaultStore {
        async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
            if self.fail_get.lock().unwrap().contains(key) {
                anyhow::bail!("injected get failure for {key}")
            }
            Ok(self.records.lock().unwrap().get(key).cloned())
        }

        async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
            if self.fail_put.lock().unwrap().contains(key) {
                anyhow::bail!("injected put failure for {key}")
            }
            self.records
                .lock()
                .unwrap()
                .insert(key.into(), bytes.to_vec());
            self.puts.lock().unwrap().push((key.into(), bytes.to_vec()));
            Ok(())
        }

        async fn delete_record(&self, key: &str) -> Result<()> {
            self.records.lock().unwrap().remove(key);
            Ok(())
        }

        async fn pull(&self, _key: &str, _dest: &Path) -> Result<bool> {
            Ok(false)
        }

        async fn push(&self, _src: &Path, _key: &str) -> Result<()> {
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
    }

    struct RecordingLauncher {
        starts: Arc<AtomicUsize>,
        stops: Arc<Mutex<Vec<String>>>,
        reconciles: Arc<AtomicUsize>,
        stop_outcome: StopOutcome,
    }

    type LauncherProbe = (
        RecordingLauncher,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
        Arc<AtomicUsize>,
    );

    #[async_trait]
    impl Launcher for RecordingLauncher {
        async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Handle {
                kind: LauncherKind::Subprocess,
                id: spec.worker_id.clone(),
            })
        }

        async fn status(&self, _handle: &Handle) -> Result<Status> {
            Ok(Status::Running)
        }

        async fn stop_and_wait(&self, handle: &Handle, _deadline: Duration) -> Result<StopOutcome> {
            self.stops.lock().unwrap().push(handle.id.clone());
            Ok(self.stop_outcome)
        }

        async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
            self.reconciles.fetch_add(1, Ordering::SeqCst);
            Ok(Some(Handle {
                kind: LauncherKind::Subprocess,
                id: spec.worker_id.clone(),
            }))
        }
    }

    fn recording_launcher(outcome: StopOutcome) -> LauncherProbe {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(Mutex::new(Vec::new()));
        let reconciles = Arc::new(AtomicUsize::new(0));
        (
            RecordingLauncher {
                starts: starts.clone(),
                stops: stops.clone(),
                reconciles: reconciles.clone(),
                stop_outcome: outcome,
            },
            starts,
            stops,
            reconciles,
        )
    }

    fn owner(affinity: crate::sandbox::Affinity, worker: &str) -> OwnerRecord {
        OwnerRecord {
            protocol_version: PROTOCOL_VERSION,
            phase: OwnerPhase::Running,
            worker_id: worker.into(),
            launch_token: format!("token-{worker}"),
            handle: Some(Handle {
                kind: LauncherKind::Subprocess,
                id: worker.into(),
            }),
            launched_at_unix: unix_now(),
            router_protocol_version: PROTOCOL_VERSION,
            policy_hash: "policy".into(),
            affinity,
        }
    }

    async fn heartbeat(
        store: &dyn StateStore,
        affinity: &str,
        worker: &str,
        phase: WorkerPhase,
        current_turn: Option<&str>,
    ) {
        put_json(
            store,
            &LaunchedWorkerProvider::heartbeat_key(affinity, worker),
            &HeartbeatRecord {
                seq: 1,
                phase,
                current_turn: current_turn.map(str::to_string),
                last_turn: None,
                protocol_version: PROTOCOL_VERSION,
                policy_hash: "policy".into(),
            },
        )
        .await
        .unwrap();
    }

    struct StubEngine {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SandboxProvider for StubEngine {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TurnResult {
                response: format!("turn-{call}"),
                backend_session_id: format!("session-{call}"),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    struct CancelThenSucceed {
        calls: Arc<AtomicUsize>,
        started: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SandboxProvider for CancelThenSucceed {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            self.started.store(true, Ordering::SeqCst);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending().await
            }
            Ok(TurnResult {
                response: "recovered".into(),
                backend_session_id: "session".into(),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    struct GatedEngine {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SandboxProvider for GatedEngine {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(TurnResult {
                response: "done".into(),
                backend_session_id: "session".into(),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    fn job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::None,
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
            attachments: Vec::new(),
        }
    }

    fn timing() -> Timing {
        Timing {
            inbox_poll: Duration::from_secs(1),
            heartbeat: Duration::from_secs(10),
            stale_after: Duration::from_secs(30),
            liveness_check: Duration::from_secs(5),
            start_timeout: Duration::from_secs(3),
            idle: Duration::from_secs(60),
            turn_timeout: Duration::from_secs(30),
            max_age: Duration::from_secs(300),
        }
    }

    async fn assign(store: &dyn StateStore, affinity: &str, worker: &str, turn: &str) {
        store
            .put_record(&job_key(turn), &serde_json::to_vec(&job()).unwrap())
            .await
            .unwrap();
        put_json(
            store,
            &LaunchedWorkerProvider::inbox_key(affinity),
            &InboxRecord {
                protocol_version: PROTOCOL_VERSION,
                turn_id: turn.into(),
                worker_id: worker.into(),
                enqueued_at_unix: 0,
            },
        )
        .await
        .unwrap();
    }

    async fn start_loop() -> (
        tempfile::TempDir,
        Arc<FilesystemStateStore>,
        tokio::task::JoinHandle<Result<()>>,
        Arc<AtomicUsize>,
        String,
        String,
    ) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let affinity = job().affinity.id();
        let worker = "worker-1".to_string();
        let owner = OwnerRecord {
            protocol_version: PROTOCOL_VERSION,
            phase: OwnerPhase::Running,
            worker_id: worker.clone(),
            launch_token: "token".into(),
            handle: None,
            launched_at_unix: 0,
            router_protocol_version: PROTOCOL_VERSION,
            policy_hash: "policy".into(),
            affinity: job().affinity,
        };
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity),
            &owner,
        )
        .await
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = WarmHydratingProvider::new(
            StubEngine {
                calls: calls.clone(),
            },
            store.clone(),
            root.path().join("claude"),
            root.path().join("cursor"),
            root.path().join("cwd"),
            None,
        );
        let spec = WorkerSpec {
            session: affinity.clone(),
            worker_id: worker.clone(),
            launch_token: "token".into(),
            idle: timing().idle,
            turn_timeout: timing().turn_timeout,
            start_timeout: timing().start_timeout,
            policy_hash: "policy".into(),
        };
        let task =
            tokio::spawn(
                async move { run_worker_loop(store.clone(), &engine, spec, timing()).await },
            );
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        tokio::task::yield_now().await;
        (root, store, task, calls, affinity, worker)
    }

    async fn spawn_loop<P: SandboxProvider + 'static>(
        root: &Path,
        store: Arc<dyn StateStore>,
        engine: P,
        worker_timing: Timing,
    ) -> (tokio::task::JoinHandle<Result<()>>, String, String) {
        let affinity = job().affinity.id();
        let worker = "worker-1".to_string();
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity),
            &owner(job().affinity, &worker),
        )
        .await
        .unwrap();
        let warm = WarmHydratingProvider::new(
            engine,
            store.clone(),
            root.join("claude"),
            root.join("cursor"),
            root.join("cwd"),
            Some((affinity.clone(), worker.clone())),
        );
        let spec = WorkerSpec {
            session: affinity.clone(),
            worker_id: worker.clone(),
            launch_token: "token".into(),
            idle: worker_timing.idle,
            turn_timeout: worker_timing.turn_timeout,
            start_timeout: worker_timing.start_timeout,
            policy_hash: "policy".into(),
        };
        let task =
            tokio::spawn(async move { run_worker_loop(store, &warm, spec, worker_timing).await });
        tokio::task::yield_now().await;
        (task, affinity, worker)
    }

    #[tokio::test(start_paused = true)]
    async fn the_warm_loop_ships_a_file_the_agent_produced() {
        let root = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(FilesystemStateStore::new(root.path().join("store")));

        let produced = root.path().join("export.json");
        std::fs::write(&produced, br#"{"herd": 120}"#).unwrap();

        struct Producing(String);
        #[async_trait::async_trait]
        impl crate::sandbox::SandboxProvider for Producing {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                Ok(TurnResult {
                    response: format!("[attachment:{}]\n\nHere it is.", self.0),
                    backend_session_id: "sess".into(),
                    cost_usd: None,
                    duration_ms: None,
                    produced_files: Vec::new(),
                })
            }
        }

        let engine = Producing(produced.display().to_string());
        let (task, affinity, worker) =
            spawn_loop(root.path(), store.clone(), engine, timing()).await;
        assign(store.as_ref(), &affinity, &worker, "t-warm").await;

        for _ in 0..200 {
            if store
                .get_record(&result_key("t-warm"))
                .await
                .ok()
                .flatten()
                .is_some()
            {
                break;
            }
            tokio::time::advance(Duration::from_millis(200)).await;
            tokio::task::yield_now().await;
        }

        let bytes = store
            .get_record(&result_key("t-warm"))
            .await
            .unwrap()
            .expect("the loop wrote a result");
        let envelope: TurnEnvelope = serde_json::from_slice(&bytes).unwrap();
        let TurnOutcome::Result(result) = envelope.outcome else {
            panic!("expected a result, got {:?}", envelope.outcome);
        };
        assert_eq!(
            result.produced_files,
            vec!["export.json".to_string()],
            "the warm loop dropped the produced file, so the marker reaches the user as text"
        );

        let dest = root.path().join("pulled");
        assert!(
            store.pull(&produced_key("t-warm"), &dest).await.unwrap(),
            "produced files were named but never pushed"
        );
        assert_eq!(
            std::fs::read(dest.join("export.json")).unwrap(),
            br#"{"herd": 120}"#
        );

        task.abort();
    }

    /// A turn that times out returns no result, so nothing is persisted -- and
    /// then `abandon` deletes the local session. The one turn worth reading was
    /// the one that left nothing at all: a 900s timeout had to be diagnosed from
    /// router logs and inference, because no transcript survived it anywhere.
    ///
    /// Drives the loop rather than calling preserve_abandoned directly, because
    /// what matters is that it runs BEFORE the discard.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_turn_leaves_its_transcript_behind() {
        let root = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(FilesystemStateStore::new(root.path().join("store")));

        // The transcript Claude Code would have written while the turn ran.
        let claude_home = root.path().join("claude");
        let transcript = claude_home.join(".claude/projects/-work/sess-timeout.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, r#"{"partial":"work in progress"}"#).unwrap();

        struct NeverReturns;
        #[async_trait::async_trait]
        impl crate::sandbox::SandboxProvider for NeverReturns {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let (task, affinity, worker) =
            spawn_loop(root.path(), store.clone(), NeverReturns, timing()).await;
        assign(store.as_ref(), &affinity, &worker, "t-timeout").await;

        // Past the turn timeout.
        for _ in 0..400 {
            if store
                .get_record(&result_key("t-timeout"))
                .await
                .ok()
                .flatten()
                .is_some()
            {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        let dest = root.path().join("recovered");
        assert!(
            store.pull("abandoned/t-timeout", &dest).await.unwrap(),
            "the timed-out turn left nothing behind — it cannot be diagnosed"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("transcript.jsonl")).unwrap(),
            r#"{"partial":"work in progress"}"#,
            "preserved the wrong thing"
        );

        // And it must not be resumable: a turn cut mid-flight restored into a
        // thread would carry a broken conversation forward.
        assert!(
            !store
                .pull("session/sess-timeout", &root.path().join("as-session"))
                .await
                .unwrap(),
            "a partial turn was persisted as a resumable session"
        );

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn worker_pulls_skills_before_ready_and_not_again_on_first_turn() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(CountingStore {
            inner: FilesystemStateStore::new(root.path().join("store")),
            skill_pulls: AtomicUsize::new(0),
        });
        let skills = root.path().join("seed");
        std::fs::create_dir_all(skills.join("foo")).unwrap();
        std::fs::write(skills.join("foo/SKILL.md"), "name: foo").unwrap();
        store.push(&skills, "skills").await.unwrap();
        store
            .put_record("skills/head", br#"{"version":"one"}"#)
            .await
            .unwrap();
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            StubEngine {
                calls: Arc::new(AtomicUsize::new(0)),
            },
            timing(),
        )
        .await;
        let heartbeat: HeartbeatRecord = serde_json::from_slice(
            &store
                .get_record(&LaunchedWorkerProvider::heartbeat_key(&affinity, &worker))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(heartbeat.phase, WorkerPhase::Ready);
        assert_eq!(store.skill_pulls.load(Ordering::SeqCst), 1);
        assert!(root.path().join("cwd/skills/foo/SKILL.md").exists());
        assign(store.as_ref(), &affinity, &worker, "one").await;
        for _ in 0..50 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if store
                .get_record(&result_key("one"))
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
        }
        assert!(
            store
                .get_record(&result_key("one"))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.skill_pulls.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn second_turn_reuses_the_first_worker() {
        let (_root, store, task, calls, affinity, worker) = start_loop().await;
        assign(store.as_ref(), &affinity, &worker, "one").await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            store
                .get_record(&result_key("one"))
                .await
                .unwrap()
                .is_some()
        );
        assign(store.as_ref(), &affinity, &worker, "two").await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let envelope: TurnEnvelope =
            serde_json::from_slice(&store.get_record(&result_key("two")).await.unwrap().unwrap())
                .unwrap();
        assert_eq!(envelope.worker_id, worker);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn worker_echoes_the_router_policy_hash() {
        let (_root, store, task, _calls, affinity, worker) = start_loop().await;
        let heartbeat: HeartbeatRecord = serde_json::from_slice(
            &store
                .get_record(&LaunchedWorkerProvider::heartbeat_key(&affinity, &worker))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(heartbeat.policy_hash, "policy");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn worker_ignores_a_consumed_turn_id() {
        let (_root, store, task, calls, affinity, worker) = start_loop().await;
        assign(store.as_ref(), &affinity, &worker, "same").await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_worker_drains_and_deletes_its_heartbeat() {
        let (_root, store, task, _calls, affinity, worker) = start_loop().await;
        tokio::time::advance(Duration::from_secs(65)).await;
        tokio::task::yield_now().await;
        task.await.unwrap().unwrap();
        assert!(
            store
                .get_record(&LaunchedWorkerProvider::heartbeat_key(&affinity, &worker))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cap_never_evicts_a_busy_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, stops, _) = recording_launcher(StopOutcome::Terminated);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            1,
        );
        let existing = crate::sandbox::Affinity::Cron {
            job_id: "existing".into(),
        };
        let existing_id = existing.id();
        provider.owners.lock().await.insert(
            existing_id.clone(),
            CachedOwner {
                record: owner(existing, "busy"),
                last_dispatch: Instant::now(),
            },
        );
        heartbeat(
            store.as_ref(),
            &existing_id,
            "busy",
            WorkerPhase::Running,
            Some("turn"),
        )
        .await;
        let error = provider
            .ensure_worker(&crate::sandbox::Affinity::Cron {
                job_id: "new".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "all workers busy");
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(stops.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn fake_store_injects_get_and_put_failures_per_key() {
        let store = FaultStore::default();
        store.put_record("healthy", b"value").await.unwrap();
        store.fail_put.lock().unwrap().insert("blocked".into());
        store.fail_get.lock().unwrap().insert("healthy".into());
        assert!(store.put_record("blocked", b"value").await.is_err());
        assert!(store.get_record("healthy").await.is_err());
        assert!(store.get_record("other").await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn cap_stops_the_lru_idle_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, stops, _) = recording_launcher(StopOutcome::Terminated);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            1,
        );
        let existing = crate::sandbox::Affinity::Cron {
            job_id: "old".into(),
        };
        let existing_id = existing.id();
        provider.owners.lock().await.insert(
            existing_id.clone(),
            CachedOwner {
                record: owner(existing, "idle"),
                last_dispatch: Instant::now(),
            },
        );
        heartbeat(
            store.as_ref(),
            &existing_id,
            "idle",
            WorkerPhase::Ready,
            None,
        )
        .await;
        provider
            .ensure_worker(&crate::sandbox::Affinity::Cron {
                job_id: "new".into(),
            })
            .await
            .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(&*stops.lock().unwrap(), &["idle"]);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_stop_outcome_fails_the_turn_without_a_second_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, _, _) = recording_launcher(StopOutcome::Unknown);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            2,
        );
        let affinity = job().affinity;
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity.id()),
            &owner(affinity.clone(), "lost"),
        )
        .await
        .unwrap();
        let error = provider.ensure_worker(&affinity).await.unwrap_err();
        assert!(error.to_string().contains("worker state unknown"));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn policy_mismatch_replaces_the_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, stops, _) = recording_launcher(StopOutcome::Terminated);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            2,
        );
        let affinity = job().affinity;
        let mut mismatched = owner(affinity.clone(), "old");
        mismatched.policy_hash = "other".into();
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity.id()),
            &mismatched,
        )
        .await
        .unwrap();
        provider.ensure_worker(&affinity).await.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(&*stops.lock().unwrap(), &["old"]);
    }

    #[tokio::test(start_paused = true)]
    async fn launching_owner_is_reconciled_not_relaunched() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, _, reconciles) = recording_launcher(StopOutcome::Terminated);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            2,
        );
        let affinity = job().affinity;
        let mut launching = owner(affinity.clone(), "adopted");
        launching.phase = OwnerPhase::Launching;
        launching.handle = None;
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity.id()),
            &launching,
        )
        .await
        .unwrap();
        heartbeat(
            store.as_ref(),
            &affinity.id(),
            "adopted",
            WorkerPhase::Ready,
            None,
        )
        .await;
        let adopted = provider.ensure_worker(&affinity).await.unwrap();
        assert_eq!(adopted.worker_id, "adopted");
        assert_eq!(reconciles.load(Ordering::SeqCst), 1);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_heartbeat_replaces_the_worker_after_confirmed_stop() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, stops, _) = recording_launcher(StopOutcome::Terminated);
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            timing(),
            "policy".into(),
            2,
        );
        let affinity = job().affinity;
        let affinity_id = affinity.id();
        let old = owner(affinity.clone(), "stale");
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity_id),
            &old,
        )
        .await
        .unwrap();
        heartbeat(
            store.as_ref(),
            &affinity_id,
            "stale",
            WorkerPhase::Ready,
            None,
        )
        .await;
        provider.ensure_worker(&affinity).await.unwrap();
        tokio::time::advance(timing().stale_after + Duration::from_secs(1)).await;
        provider.ensure_worker(&affinity).await.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(&*stops.lock().unwrap(), &["stale"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_record_aborts_the_turn_and_keeps_the_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            CancelThenSucceed {
                calls: calls.clone(),
                started: started.clone(),
            },
            timing(),
        )
        .await;
        assign(store.as_ref(), &affinity, &worker, "cancelled").await;
        for _ in 0..50 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if started.load(Ordering::SeqCst) {
                break;
            }
        }
        assert!(started.load(Ordering::SeqCst));
        store
            .put_record("turns/cancelled/cancel", b"{}")
            .await
            .unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            let heartbeat: HeartbeatRecord = serde_json::from_slice(
                &store
                    .get_record(&LaunchedWorkerProvider::heartbeat_key(&affinity, &worker))
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            if heartbeat.current_turn.is_none() {
                break;
            }
        }
        assert!(
            store
                .get_record(&result_key("cancelled"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_record(&LaunchedWorkerProvider::heartbeat_key(&affinity, &worker))
                .await
                .unwrap()
                .is_some()
        );
        assign(store.as_ref(), &affinity, &worker, "next").await;
        for _ in 0..50 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if store
                .get_record(&result_key("next"))
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            store
                .get_record(&result_key("next"))
                .await
                .unwrap()
                .is_some()
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn fenced_worker_skips_dehydrate_and_result() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            GatedEngine {
                started: started.clone(),
                release: release.clone(),
            },
            timing(),
        )
        .await;
        assign(store.as_ref(), &affinity, &worker, "fenced").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        started.notified().await;
        let mut replacement = owner(job().affinity, "replacement");
        replacement.policy_hash = "policy".into();
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity),
            &replacement,
        )
        .await
        .unwrap();
        release.notify_one();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            store
                .get_record(&result_key("fenced"))
                .await
                .unwrap()
                .is_none()
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn max_age_drains_after_the_active_turn() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut worker_timing = timing();
        worker_timing.max_age = Duration::from_secs(2);
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            GatedEngine {
                started: started.clone(),
                release: release.clone(),
            },
            worker_timing,
        )
        .await;
        assign(store.as_ref(), &affinity, &worker, "active").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        started.notified().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(!task.is_finished());
        release.notify_one();
        tokio::time::advance(Duration::from_secs(2)).await;
        task.await.unwrap().unwrap();
        assert!(
            store
                .get_record(&result_key("active"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completion_heartbeat_stays_draining() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FaultStore::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut worker_timing = timing();
        worker_timing.max_age = Duration::from_secs(2);
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            GatedEngine {
                started: started.clone(),
                release: release.clone(),
            },
            worker_timing,
        )
        .await;
        assign(store.as_ref(), &affinity, &worker, "active").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        started.notified().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        release.notify_one();
        tokio::time::advance(Duration::from_secs(2)).await;
        task.await.unwrap().unwrap();
        let key = LaunchedWorkerProvider::heartbeat_key(&affinity, &worker);
        let completion = store
            .puts
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|(put_key, _)| put_key == &key)
            .filter_map(|(_, bytes)| serde_json::from_slice::<HeartbeatRecord>(bytes).ok())
            .find(|heartbeat| heartbeat.last_turn.as_deref() == Some("active"))
            .unwrap();
        assert_eq!(completion.phase, WorkerPhase::Draining);
    }

    #[tokio::test(start_paused = true)]
    async fn job_landing_during_drain_is_run_or_fails_never_lost_silently() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut worker_timing = timing();
        worker_timing.idle = Duration::from_secs(2);
        let (task, affinity, worker) = spawn_loop(
            root.path(),
            store.clone(),
            StubEngine {
                calls: calls.clone(),
            },
            worker_timing,
        )
        .await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assign(store.as_ref(), &affinity, &worker, "edge").await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = store.get_record(&result_key("edge")).await.unwrap();
        assert!(result.is_some() || task.is_finished());
        if !task.is_finished() {
            task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn vanished_worker_fails_the_turn_and_never_redispatches() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let (launcher, starts, _, _) = recording_launcher(StopOutcome::Terminated);
        let mut router_timing = timing();
        router_timing.liveness_check = Duration::from_secs(1);
        let provider = Arc::new(LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            root.path().join("base"),
            router_timing,
            "policy".into(),
            2,
        ));
        let affinity = job().affinity;
        let affinity_id = affinity.id();
        let live = owner(affinity.clone(), "vanished");
        put_json(
            store.as_ref(),
            &LaunchedWorkerProvider::owner_key(&affinity_id),
            &live,
        )
        .await
        .unwrap();
        heartbeat(
            store.as_ref(),
            &affinity_id,
            "vanished",
            WorkerPhase::Ready,
            None,
        )
        .await;
        let turn = tokio::spawn({
            let provider = provider.clone();
            async move { provider.run_turn(job()).await }
        });
        tokio::task::yield_now().await;
        store
            .delete_record(&LaunchedWorkerProvider::heartbeat_key(
                &affinity_id,
                "vanished",
            ))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        let error = turn.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "worker vanished during turn");
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }
}
