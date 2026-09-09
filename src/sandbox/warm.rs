use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tracing::warn;

use crate::config::AiBackend;
use crate::sandbox::artifacts::{ClaudeSessionArtifacts, CursorSessionArtifacts, SessionArtifacts};
use crate::sandbox::state::StateStore;
use crate::sandbox::{SandboxProvider, SessionPersistence, TurnJob, TurnResult};

#[derive(serde::Deserialize)]
struct SkillsHead {
    version: String,
}

/// Worker-only hydration that retains safe session state between turns.
pub struct WarmHydratingProvider<P: SandboxProvider> {
    inner: P,
    store: Arc<dyn StateStore>,
    claude_home: PathBuf,
    cursor_home: PathBuf,
    cwd: PathBuf,
    owned_sessions: Mutex<HashSet<String>>,
    skills_version: Mutex<Option<String>>,
    fence: Option<(String, String)>,
}

impl<P: SandboxProvider> WarmHydratingProvider<P> {
    pub fn new(
        inner: P,
        store: Arc<dyn StateStore>,
        claude_home: PathBuf,
        cursor_home: PathBuf,
        cwd: PathBuf,
        fence: Option<(String, String)>,
    ) -> Self {
        Self {
            inner,
            store,
            claude_home,
            cursor_home,
            cwd,
            owned_sessions: Mutex::new(HashSet::new()),
            skills_version: Mutex::new(None),
            fence,
        }
    }

    fn staging(&self) -> PathBuf {
        std::env::temp_dir().join(format!("cica-warm-{}", uuid::Uuid::new_v4()))
    }

    fn memories_dir(&self, job: &TurnJob) -> PathBuf {
        self.cwd
            .join("users")
            .join(format!("{}_{}", job.channel, job.user_id))
            .join("memories")
    }

    fn forget_local(&self, backend: AiBackend, session: &str) {
        self.owned_sessions.lock().unwrap().remove(session);
        let (artifacts, home): (&dyn SessionArtifacts, &Path) = match backend {
            AiBackend::Claude => (&ClaudeSessionArtifacts, &self.claude_home),
            AiBackend::Cursor => (&CursorSessionArtifacts, &self.cursor_home),
        };
        if let Err(error) = artifacts.forget(home, session) {
            warn!("failed to discard local session {session}: {error}");
        }
    }

    pub fn abandon(&self, job: &TurnJob) {
        if let Some(session) = &job.resume_session {
            self.forget_local(job.backend, session);
        }
    }

    /// Save what an abandoned turn left behind, before `abandon` discards it.
    ///
    /// A turn that times out returns no result, so the dehydrate step below
    /// never runs and nothing is persisted -- and then `forget_local` deletes
    /// the local copy. The one turn we most need to read is the one that
    /// leaves nothing at all, which is how a 900s timeout was diagnosed from
    /// router logs and inference.
    ///
    /// Deliberately **not** `session/<id>`: a turn cut mid-flight must never
    /// become resumable, or the next message in that thread restores a broken
    /// conversation. This is a copy for humans, under `abandoned/<turn_id>`,
    /// and nothing reads it back.
    ///
    /// Best-effort throughout. The turn has already failed; failing to file the
    /// evidence must not make that worse.
    pub async fn preserve_abandoned(&self, job: &TurnJob, turn_id: &str) {
        let backend = job.backend;
        let home = match backend {
            AiBackend::Claude => self.claude_home.clone(),
            AiBackend::Cursor => self.cursor_home.clone(),
        };
        let resume = job.resume_session.clone();
        let scratch = self.staging();
        let captured = tokio::task::spawn_blocking(move || -> Result<_> {
            let artifacts: &dyn SessionArtifacts = match backend {
                AiBackend::Claude => &ClaudeSessionArtifacts,
                AiBackend::Cursor => &CursorSessionArtifacts,
            };
            // A resumed turn knows its session. A fresh one never reported an id,
            // so fall back to whatever was written last.
            let Some(session) = resume.or_else(|| artifacts.latest_session(&home)) else {
                return Ok(None);
            };
            let staging = crate::atomic::Staging::beside(&scratch)?;
            if artifacts.capture(&home, &session, staging.path())? {
                Ok(Some((session, staging)))
            } else {
                Ok(None)
            }
        })
        .await;
        match captured {
            Ok(Ok(Some((session, staging)))) => {
                let key = format!("abandoned/{turn_id}");
                match self.store.push_archive(staging.path(), &key).await {
                    Ok(()) => warn!(
                        "turn {turn_id} abandoned; partial transcript for session {session} preserved at {key}"
                    ),
                    Err(error) => {
                        warn!("failed to preserve the abandoned turn {turn_id}: {error}")
                    }
                }
            }
            Ok(Ok(None)) => warn!("turn {turn_id} abandoned with no transcript to preserve"),
            Ok(Err(error)) => warn!("failed to capture the abandoned turn {turn_id}: {error}"),
            Err(error) => warn!("abandoned transcript capture task failed for {turn_id}: {error}"),
        }
    }

    pub async fn warm_up(&self) {
        self.refresh_skills().await;
    }

    async fn refresh_skills(&self) {
        let head = match self.store.get_record("skills/head").await {
            Ok(Some(bytes)) => serde_json::from_slice::<SkillsHead>(&bytes).map_err(Into::into),
            Ok(None) => return,
            Err(error) => Err(error),
        };
        let head = match head {
            Ok(head) => head,
            Err(error) => {
                warn!("failed to read skills head; keeping last-good skills: {error}");
                return;
            }
        };
        if self.skills_version.lock().unwrap().as_deref() == Some(&head.version) {
            return;
        }
        match self.store.pull("skills", &self.cwd.join("skills")).await {
            Ok(true) => *self.skills_version.lock().unwrap() = Some(head.version),
            Ok(false) => {
                warn!("skills head exists without a skills tree; keeping last-good skills")
            }
            Err(error) => warn!("failed to pull changed skills; keeping last-good skills: {error}"),
        }
    }
}

#[async_trait]
impl<P: SandboxProvider> SandboxProvider for WarmHydratingProvider<P> {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        self.refresh_skills().await;
        let persist = job.session_persistence == SessionPersistence::Resume;
        let (artifacts, home): (Box<dyn SessionArtifacts + Send>, &Path) = match job.backend {
            AiBackend::Claude => (Box::new(ClaudeSessionArtifacts), &self.claude_home),
            AiBackend::Cursor => (Box::new(CursorSessionArtifacts), &self.cursor_home),
        };
        for relative in &job.attachments {
            let staging = self.staging();
            match self
                .store
                .pull(&format!("attachments/{relative}"), &staging)
                .await
            {
                Ok(true) => {
                    if let Some(name) = Path::new(relative).file_name() {
                        let dest = self.cwd.join(relative);
                        let copied = dest
                            .parent()
                            .map(std::fs::create_dir_all)
                            .transpose()
                            .and_then(|_| std::fs::copy(staging.join(name), dest).map(|_| ()));
                        if let Err(error) = copied {
                            warn!("failed to hydrate attachment {relative}: {error}");
                        }
                    }
                }
                Ok(false) => {
                    warn!("attachment {relative} is not in the store; the agent will not see it")
                }
                Err(error) => warn!("failed to pull attachment {relative}: {error}"),
            }
            let _ = std::fs::remove_dir_all(staging);
        }
        if persist
            && let Some(session) = &job.resume_session
            && !self.owned_sessions.lock().unwrap().contains(session)
        {
            let staging = self.staging();
            match self
                .store
                .pull(&format!("session/{session}"), &staging)
                .await
            {
                Ok(true) => {
                    if let Err(error) = artifacts.restore(home, &self.cwd, session, &staging) {
                        warn!(
                            "failed to restore session {session}; backend will start fresh: {error}"
                        );
                    }
                }
                Ok(false) => {
                    warn!("session {session} is not in the store; backend will start fresh")
                }
                Err(error) => {
                    warn!("failed to pull session {session}; backend will start fresh: {error}")
                }
            }
            let _ = std::fs::remove_dir_all(staging);
        }
        let mem_key = format!("mem/{}_{}", job.channel, job.user_id);
        let mem_dir = self.memories_dir(&job);
        let memories_hydrated = match self.store.pull(&mem_key, &mem_dir).await {
            Ok(_) => true,
            Err(error) => {
                warn!("failed to pull {mem_key}; not persisting memories: {error}");
                false
            }
        };
        let backend = job.backend;
        let resume = job.resume_session.clone();
        let result = match self.inner.run_turn(job).await {
            Ok(result) => result,
            Err(error) => {
                if let Some(session) = resume.as_deref() {
                    self.forget_local(backend, session);
                }
                return Err(error);
            }
        };
        if let Some((session, worker_id)) = &self.fence {
            let owned = match self
                .store
                .get_record(&format!("sessions/{session}/owner"))
                .await
            {
                Ok(Some(bytes)) => {
                    serde_json::from_slice::<crate::sandbox::worker::OwnerRecord>(&bytes)
                        .is_ok_and(|owner| owner.worker_id == *worker_id)
                }
                Ok(None) => false,
                Err(error) => {
                    warn!(
                        "failed to verify worker ownership before dehydration; skipping persistence: {error}"
                    );
                    false
                }
            };
            if !owned {
                if let Some(resume) = resume.as_deref() {
                    self.forget_local(backend, resume);
                }
                return Ok(result);
            }
        }
        if persist && !result.backend_session_id.is_empty() {
            let session = &result.backend_session_id;
            let staging = self.staging();
            match artifacts.capture(home, session, &staging) {
                Ok(true) => match self
                    .store
                    .push(&staging, &format!("session/{session}"))
                    .await
                {
                    Ok(()) => {
                        self.owned_sessions.lock().unwrap().insert(session.clone());
                    }
                    Err(error) => {
                        warn!("failed to persist session {session}; reply still delivered: {error}")
                    }
                },
                Ok(false) => {}
                Err(error) => warn!("failed to capture session {session}: {error}"),
            }
            let _ = std::fs::remove_dir_all(staging);
        }
        if memories_hydrated
            && mem_dir.exists()
            && let Err(error) = self.store.push(&mem_dir, &mem_key).await
        {
            warn!("failed to persist memories; reply still delivered: {error}");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::FilesystemStateStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct Success;

    #[async_trait]
    impl SandboxProvider for Success {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            Ok(TurnResult {
                response: "ok".into(),
                backend_session_id: String::new(),
                cost_usd: None,
                duration_ms: None,
                produced_files: Vec::new(),
            })
        }
    }

    struct Failure;

    #[async_trait]
    impl SandboxProvider for Failure {
        async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
            anyhow::bail!("failed")
        }
    }

    fn job(resume: Option<&str>) -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: SessionPersistence::Resume,
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: resume.map(str::to_string),
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
            attachments: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn skills_head_change_triggers_one_pull() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(CountingStore {
            inner: FilesystemStateStore::new(root.path().join("store")),
            skill_pulls: AtomicUsize::new(0),
        });
        let skills = root.path().join("seed");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("SKILL.md"), "skill").unwrap();
        store.push(&skills, "skills").await.unwrap();
        store
            .put_record("skills/head", br#"{"version":"one"}"#)
            .await
            .unwrap();
        let provider = WarmHydratingProvider::new(
            Success,
            store.clone(),
            root.path().join("claude"),
            root.path().join("cursor"),
            root.path().join("cwd"),
            None,
        );
        provider.run_turn(job(None)).await.unwrap();
        provider.run_turn(job(None)).await.unwrap();
        assert_eq!(store.skill_pulls.load(Ordering::SeqCst), 1);
        store
            .put_record("skills/head", br#"{"version":"two"}"#)
            .await
            .unwrap();
        provider.run_turn(job(None)).await.unwrap();
        assert_eq!(store.skill_pulls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn worker_without_skills_config_still_pulls_published_skills() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(CountingStore {
            inner: FilesystemStateStore::new(root.path().join("store")),
            skill_pulls: AtomicUsize::new(0),
        });
        let skills = root.path().join("seed");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(skills.join("foo")).unwrap();
        std::fs::write(skills.join("foo/SKILL.md"), "name: foo").unwrap();
        store.push(&skills, "skills").await.unwrap();
        store
            .put_record("skills/head", br#"{"version":"one"}"#)
            .await
            .unwrap();
        let cwd = root.path().join("cwd");
        let provider = WarmHydratingProvider::new(
            Success,
            store.clone(),
            root.path().join("claude"),
            root.path().join("cursor"),
            cwd.clone(),
            None,
        );
        provider.run_turn(job(None)).await.unwrap();
        assert_eq!(store.skill_pulls.load(Ordering::SeqCst), 1);
        assert!(cwd.join("skills/foo/SKILL.md").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn session_is_cold_after_a_failed_turn() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().join("store")));
        let claude = root.path().join("claude");
        let transcript = claude.join(".claude/projects/work/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "state").unwrap();
        let provider = WarmHydratingProvider::new(
            Failure,
            store,
            claude,
            root.path().join("cursor"),
            root.path().join("cwd"),
            None,
        );
        assert!(provider.run_turn(job(Some("session"))).await.is_err());
        assert!(!transcript.exists());
        assert!(!provider.owned_sessions.lock().unwrap().contains("session"));
    }
}
