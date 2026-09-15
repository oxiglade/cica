//! Cron job scheduling system for automated Claude Code tasks.

mod clock;
mod schedule;
pub mod store;

pub use clock::{Clock, SystemClock};
pub use schedule::CronSchedule;
pub use store::{CronJob, CronStore, DeliveryTarget, JobId, JobStatus};

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audit;
use crate::backends::QueryResult;
use crate::channels::get_channel_info;
use crate::config::{Config, Paths};
use crate::onboarding;
use crate::runtime::lock;
use crate::sandbox::{self, SandboxProvider, TurnJob};

/// Configuration for the cron service.
#[derive(Clone)]
pub struct CronConfig {
    /// Tick interval - how often to check for due jobs (default: 60 seconds).
    pub tick_interval: Duration,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(60),
        }
    }
}

/// (channel, user_id, target, message) -> Result<()>
pub type ResultSender = Arc<
    dyn Fn(
            String,
            String,
            DeliveryTarget,
            String,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// The cron service - manages scheduled job execution.
pub struct CronService<C: Clock> {
    clock: C,
    store: Arc<Mutex<CronStore>>,
    config: CronConfig,
    shutdown_tx: Mutex<Option<mpsc::Sender<()>>>,
    app_config: Arc<Config>,
    paths: Arc<Paths>,
    provider: Arc<dyn SandboxProvider>,
}

impl<C: Clock> CronService<C> {
    /// Create a new cron service.
    pub fn new(
        clock: C,
        config: CronConfig,
        app_config: Arc<Config>,
        paths: Arc<Paths>,
        provider: Arc<dyn SandboxProvider>,
    ) -> Result<Self> {
        let mut store = CronStore::load(&paths)?;

        let recovered = store.recover_stuck_jobs(clock.now_millis());
        if recovered > 0 {
            info!(
                "Recovered {} stuck cron job(s) from previous run",
                recovered
            );
            store.modify(|store| {
                store.recover_stuck_jobs(clock.now_millis());
                Ok(())
            })?;
        }

        Ok(Self {
            clock,
            store: Arc::new(Mutex::new(store)),
            config,
            shutdown_tx: Mutex::new(None),
            app_config,
            paths,
            provider,
        })
    }

    /// Start the scheduler loop (spawns background task).
    /// Returns a JoinHandle that can be awaited for shutdown.
    pub fn start(&self, result_sender: ResultSender) -> tokio::task::JoinHandle<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        *lock(&self.shutdown_tx) = Some(shutdown_tx);

        let clock = self.clock.clone();
        let store = Arc::clone(&self.store);
        let tick_interval = self.config.tick_interval;
        let app_config = self.app_config.clone();
        let paths = self.paths.clone();
        let provider = self.provider.clone();

        tokio::spawn(async move {
            info!(
                "Cron scheduler started (tick interval: {:?})",
                tick_interval
            );

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("Cron scheduler shutting down");
                        break;
                    }
                    _ = clock.sleep(tick_interval) => {
                        let now = clock.now_millis();
                        let due_jobs = {
                            let mut store_guard = lock(&store);
                            if let Err(e) = store_guard.reload() {
                                warn!("Failed to reload cron store: {}", e);
                            }
                            store_guard.get_due_jobs(now)
                                .iter()
                                .map(|j| (*j).clone())
                                .collect::<Vec<_>>()
                        };

                        if !due_jobs.is_empty() {
                            debug!("Found {} due cron jobs", due_jobs.len());
                        }

                        for job in due_jobs {
                            let store = Arc::clone(&store);
                            let result_sender = result_sender.clone();
                            let clock = clock.clone();
                            let app_config = app_config.clone();
                            let paths = paths.clone();
                            let provider = provider.clone();

                            tokio::spawn(async move {
                                execute_job(job, store, result_sender, &clock, &app_config, &paths, provider.as_ref()).await;
                            });
                        }
                    }
                }
            }
        })
    }

    /// Stop the scheduler.
    pub fn stop(&self) {
        if let Some(tx) = lock(&self.shutdown_tx).take() {
            let _ = tx.try_send(());
        }
    }

    /// Add a new job.
    pub fn add(
        &self,
        name: String,
        prompt: String,
        schedule: CronSchedule,
        channel: String,
        user_id: String,
        target: Option<DeliveryTarget>,
    ) -> Result<CronJob> {
        let job = CronJob::new(name, prompt, schedule, channel, user_id, target);
        lock(&self.store).modify(|store| {
            store.add(job.clone())?;
            Ok(job)
        })
    }

    /// Remove a job.
    pub fn remove(&self, id: &str, channel: &str, user_id: &str) -> Result<Option<CronJob>> {
        lock(&self.store).modify(|store| store.remove(id, channel, user_id))
    }

    /// List jobs for a user.
    pub fn list(&self, channel: &str, user_id: &str) -> Vec<CronJob> {
        let store = lock(&self.store);
        store
            .list_for_user(channel, user_id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get job status.
    pub fn status(&self, id: &str, channel: &str, user_id: &str) -> Option<CronJob> {
        let store = lock(&self.store);
        store.get(id, channel, user_id).cloned()
    }

    pub fn set_enabled(
        &self,
        id: &str,
        channel: &str,
        user_id: &str,
        enabled: bool,
    ) -> Result<CronJob> {
        lock(&self.store).modify(|store| {
            store
                .get(id, channel, user_id)
                .ok_or_else(|| anyhow::anyhow!("Job not found: {}", id))?;
            let job = store.jobs.get_mut(id).expect("job was just found");
            job.enabled = enabled;
            if enabled {
                job.update_next_run(self.clock.now_millis());
            } else {
                job.state.next_run_at = None;
            }
            Ok(job.clone())
        })
    }

    pub fn resolve_id(&self, channel: &str, user_id: &str, id_or_prefix: &str) -> Result<JobId> {
        let store = lock(&self.store);
        let id = id_or_prefix.trim();
        if store.get(id, channel, user_id).is_some() {
            return Ok(id.to_string());
        }
        let matches: Vec<_> = store
            .list_for_user(channel, user_id)
            .into_iter()
            .filter(|j| j.id.starts_with(id))
            .collect();
        match matches.len() {
            0 => anyhow::bail!("Job not found: {}", id),
            1 => Ok(matches[0].id.clone()),
            _ => anyhow::bail!(
                "Ambiguous job ID '{}'. Matches: {}",
                id,
                matches
                    .iter()
                    .map(|j| j.short_id())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Execute a single job.
async fn execute_job<C: Clock>(
    job: CronJob,
    store: Arc<Mutex<CronStore>>,
    result_sender: ResultSender,
    clock: &C,
    config: &Config,
    paths: &Paths,
    provider: &dyn SandboxProvider,
) {
    let job_id = job.id.clone();
    info!("Executing cron job: {} ({})", job.name, job.short_id());

    let start_time = clock.now_millis();

    // Mark as running and clear next_run_at to prevent duplicate execution
    {
        let mut store = lock(&store);
        if let Err(e) = store.update_job(&job_id, |job| {
            job.state.last_status = JobStatus::Running;
            job.state.next_run_at = None;
        }) {
            warn!("Failed to mark cron job {} running: {}", job_id, e);
        }
    }

    // Build context prompt so the job has access to skills, configs, etc.
    let channel_display = get_channel_info(&job.channel).map(|c| c.display_name);
    let context_prompt = onboarding::build_context_prompt_for_user(
        config,
        paths,
        channel_display,
        Some(&job.channel),
        Some(&job.user_id),
        Some(&job.prompt),
    );

    let result = match context_prompt {
        Ok(ctx) => {
            let turn = TurnJob::new(
                config,
                &job.channel,
                &job.user_id,
                crate::sandbox::Affinity::Cron {
                    job_id: job.id.clone(),
                },
                job.prompt.clone(),
                Some(ctx),
                None,
            );
            let turn = TurnJob {
                session_persistence: crate::sandbox::SessionPersistence::None,
                ..turn
            };
            provider
                .run_turn(turn)
                .await
                .map(sandbox::query_result_from_turn)
        }
        Err(e) => Err(e),
    };

    let end_time = clock.now_millis();
    let duration_ms = end_time - start_time;

    let status_str = if result.is_ok() { "success" } else { "failed" };
    audit::log_event(
        "cron_executed",
        Some(&job.channel),
        Some(&job.user_id),
        Some(&format!(
            "{{\"job_id\":\"{}\",\"job_name\":\"{}\",\"status\":\"{}\",\"duration_ms\":{}}}",
            job.id, job.name, status_str, duration_ms
        )),
    );

    {
        let mut store = lock(&store);
        if let Err(e) = store.update_job(&job_id, |stored_job| {
            stored_job.state.last_run_at = Some(end_time);
            stored_job.state.last_duration_ms = Some(duration_ms);

            match &result {
                Ok(_) => {
                    stored_job.state.last_status = JobStatus::Success;
                    stored_job.state.failure_count = 0;
                }
                Err(e) => {
                    stored_job.state.last_status = JobStatus::Failed(e.to_string());
                    stored_job.state.failure_count += 1;
                }
            }

            stored_job.update_next_run(end_time);

            // For one-shot At jobs that have completed, disable them
            if matches!(stored_job.schedule, CronSchedule::At(_)) && result.is_ok() {
                stored_job.enabled = false;
                stored_job.state.next_run_at = None;
            }
        }) {
            warn!("Failed to update completed cron job {}: {}", job_id, e);
        }
    }

    if job.notify {
        let message = match result {
            Ok(QueryResult { response, .. }) => {
                format!("[Cron: {}]\n\n{}", job.name, response)
            }
            Err(e) => {
                format!("[Cron: {} FAILED]\n\nError: {}", job.name, e)
            }
        };

        if let Err(e) = result_sender(
            job.channel.clone(),
            job.user_id.clone(),
            job.target.clone(),
            message,
        )
        .await
        {
            warn!("Failed to send cron result to user: {}", e);
        }
    }

    info!("Cron job {} completed in {}ms", job.short_id(), duration_ms);
}

/// Format a timestamp for display.
pub fn format_timestamp(ms: u64) -> String {
    DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Parse a /cron add command and return (schedule, prompt).
pub fn parse_add_command(input: &str) -> Result<(CronSchedule, String)> {
    let input = input.trim();

    if input.is_empty() {
        anyhow::bail!("Usage: /cron add <schedule> <prompt>");
    }

    // Try to find where schedule ends and prompt begins
    // Patterns: "every Xunit", "at DATETIME", or cron "* * * * *"

    if input.starts_with("every ") {
        // "every 1h prompt here"
        let parts: Vec<&str> = input.splitn(3, ' ').collect();
        if parts.len() < 3 {
            anyhow::bail!("Usage: /cron add every <interval> <prompt>");
        }
        let schedule_str = format!("{} {}", parts[0], parts[1]);
        let schedule = CronSchedule::parse(&schedule_str).map_err(|e| anyhow::anyhow!(e))?;
        let prompt = parts[2].to_string();

        return Ok((schedule, prompt));
    }

    if input.starts_with("at ") {
        // "at 2024-01-28 14:00 prompt here" - datetime is 2 words
        let parts: Vec<&str> = input.splitn(4, ' ').collect();
        if parts.len() < 4 {
            anyhow::bail!("Usage: /cron add at <date> <time> <prompt>");
        }
        let schedule_str = format!("{} {} {}", parts[0], parts[1], parts[2]);
        let schedule = CronSchedule::parse(&schedule_str).map_err(|e| anyhow::anyhow!(e))?;
        let prompt = parts[3].to_string();

        return Ok((schedule, prompt));
    }

    // Try cron expression (5 fields separated by spaces)
    let parts: Vec<&str> = input.splitn(6, ' ').collect();
    if parts.len() >= 6 {
        let cron_expr = format!(
            "{} {} {} {} {}",
            parts[0], parts[1], parts[2], parts[3], parts[4]
        );
        if let Ok(schedule) = CronSchedule::parse(&cron_expr) {
            let prompt = parts[5].to_string();
            return Ok((schedule, prompt));
        }
    }

    anyhow::bail!(
        "Could not parse schedule. Use:\n\
         - every <interval> (e.g., every 1h, every 10s)\n\
         - at <datetime> (e.g., at 2024-01-28 14:00)\n\
         - <cron expression> (e.g., 0 9 * * *)"
    )
}

/// Truncate a string for use as a job name.
pub fn truncate_for_name(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len < 3 {
        ".".repeat(max_len)
    } else {
        format!("{}...", s.chars().take(max_len - 3).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct ErrorProvider;

    #[async_trait]
    impl SandboxProvider for ErrorProvider {
        async fn run_turn(&self, _job: TurnJob) -> Result<crate::sandbox::TurnResult> {
            anyhow::bail!("unused test provider")
        }
    }

    fn service(paths: &Paths) -> CronService<SystemClock> {
        CronService::new(
            SystemClock,
            CronConfig::default(),
            Arc::new(Config::default()),
            Arc::new(paths.clone()),
            Arc::new(ErrorProvider),
        )
        .unwrap()
    }

    #[test]
    fn set_enabled_is_not_a_toggle() {
        let (_temp, paths) = crate::config::test_paths();
        let service = service(&paths);
        let job = service
            .add(
                "job".into(),
                "prompt".into(),
                CronSchedule::Every(60_000),
                "telegram".into(),
                "1".into(),
                None,
            )
            .unwrap();

        service
            .set_enabled(&job.id, "telegram", "1", false)
            .unwrap();
        let paused = service
            .set_enabled(&job.id, "telegram", "1", false)
            .unwrap();
        assert!(!paused.enabled);
        assert_eq!(paused.state.next_run_at, None);

        let resumed = service.set_enabled(&job.id, "telegram", "1", true).unwrap();
        assert!(resumed.enabled);
        assert!(resumed.state.next_run_at.is_some());
    }

    #[test]
    fn cron_init_fails_on_corrupt_file() {
        let (_temp, paths) = crate::config::test_paths();
        let cron_path = paths.base.join("cron.json");
        std::fs::write(&cron_path, "{").unwrap();
        let error = CronService::new(
            SystemClock,
            CronConfig::default(),
            Arc::new(Config::default()),
            Arc::new(paths),
            Arc::new(ErrorProvider),
        )
        .err()
        .expect("corrupt cron store must fail");
        assert!(error.to_string().contains(cron_path.to_str().unwrap()));
    }

    #[test]
    fn cron_add_during_running_job_survives_completion() {
        let (_temp, paths) = crate::config::test_paths();
        let service = service(&paths);
        let first = service
            .add(
                "first".into(),
                "prompt".into(),
                CronSchedule::Every(60_000),
                "telegram".into(),
                "1".into(),
                None,
            )
            .unwrap();
        lock(&service.store)
            .update_job(&first.id, |job| job.state.last_status = JobStatus::Running)
            .unwrap();
        let second = service
            .add(
                "second".into(),
                "prompt".into(),
                CronSchedule::Every(60_000),
                "telegram".into(),
                "1".into(),
                None,
            )
            .unwrap();
        lock(&service.store)
            .update_job(&first.id, |job| job.state.last_status = JobStatus::Success)
            .unwrap();

        assert!(service.status(&first.id, "telegram", "1").is_some());
        assert!(service.status(&second.id, "telegram", "1").is_some());
        let stored = CronStore::load(&paths).unwrap();
        assert!(stored.jobs.contains_key(&first.id));
        assert!(stored.jobs.contains_key(&second.id));
        assert_eq!(stored.jobs[&first.id].state.last_status, JobStatus::Success);
    }

    #[test]
    fn test_parse_add_every() {
        let (schedule, prompt) = parse_add_command("every 1h Check my emails").unwrap();
        assert!(matches!(schedule, CronSchedule::Every(3_600_000)));
        assert_eq!(prompt, "Check my emails");
    }

    #[test]
    fn test_parse_add_every_short() {
        let (schedule, prompt) = parse_add_command("every 10s Say hello").unwrap();
        assert!(matches!(schedule, CronSchedule::Every(10_000)));
        assert_eq!(prompt, "Say hello");
    }

    #[test]
    fn test_parse_add_cron() {
        let (schedule, prompt) = parse_add_command("0 9 * * * Good morning!").unwrap();
        assert!(matches!(schedule, CronSchedule::Cron(_)));
        assert_eq!(prompt, "Good morning!");
    }

    #[test]
    fn test_truncate_for_name() {
        assert_eq!(truncate_for_name("short", 10), "short");
        assert_eq!(truncate_for_name("this is a long name", 10), "this is...");
    }

    #[test]
    fn truncate_for_name_preserves_short_unicode_names() {
        assert_eq!(
            truncate_for_name("  東京で朝食を食べる  ", 10),
            "東京で朝食を食べる"
        );
    }

    #[test]
    fn truncate_for_name_handles_multibyte_characters_at_the_cutoff() {
        for character in ['東', '😀'] {
            let input = format!("{}{character} morning reminder", "a".repeat(26));
            assert_eq!(
                truncate_for_name(&input, 30),
                format!("{}{character}...", "a".repeat(26))
            );
        }
    }

    #[test]
    fn truncate_for_name_handles_limits_shorter_than_the_ellipsis() {
        for (max_len, expected) in [(0, ""), (1, "."), (2, ".."), (3, "...")] {
            assert_eq!(truncate_for_name("long name", max_len), expected);
        }
    }
}
