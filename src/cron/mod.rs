//! Cron job scheduling system for automated Claude Code tasks.

mod clock;
mod schedule;
pub mod store;

pub use clock::{Clock, SystemClock};
pub use schedule::CronSchedule;
pub use store::{CronJob, CronStore, JobId, JobStatus};

// Re-export for tests
#[cfg(test)]
pub use clock::FakeClock;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::channels::get_channel_info;
use crate::claude::{self, QueryOptions};
use crate::onboarding;

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

/// Type alias for the result sender callback.
/// (channel, user_id, message) -> Result<()>
pub type ResultSender = Arc<
    dyn Fn(String, String, String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// The cron service - manages scheduled job execution.
pub struct CronService<C: Clock> {
    clock: C,
    store: Arc<Mutex<CronStore>>,
    config: CronConfig,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl<C: Clock> CronService<C> {
    /// Create a new cron service.
    pub fn new(clock: C, config: CronConfig) -> Result<Self> {
        let store = CronStore::load()?;

        Ok(Self {
            clock,
            store: Arc::new(Mutex::new(store)),
            config,
            shutdown_tx: None,
        })
    }

    /// Start the scheduler loop (spawns background task).
    /// Returns a JoinHandle that can be awaited for shutdown.
    pub fn start(&mut self, result_sender: ResultSender) -> tokio::task::JoinHandle<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        let clock = self.clock.clone();
        let store = Arc::clone(&self.store);
        let tick_interval = self.config.tick_interval;

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
                        // Reload store from disk to pick up external changes
                        // (e.g., agent modifying cron.json directly)
                        {
                            let mut store_guard = store.lock().await;
                            match CronStore::load() {
                                Ok(fresh) => *store_guard = fresh,
                                Err(e) => warn!("Failed to reload cron store: {}", e),
                            }
                        }

                        // Check for due jobs
                        let now = clock.now_millis();
                        let due_jobs = {
                            let store = store.lock().await;
                            store.get_due_jobs(now)
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

                            tokio::spawn(async move {
                                execute_job(job, store, result_sender, &clock).await;
                            });
                        }
                    }
                }
            }
        })
    }

    /// Stop the scheduler.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
    }

    /// Add a new job.
    #[allow(dead_code)]
    pub async fn add(
        &self,
        name: String,
        prompt: String,
        schedule: CronSchedule,
        channel: String,
        user_id: String,
    ) -> Result<JobId> {
        let job = CronJob::new(name, prompt, schedule, channel, user_id);
        let mut store = self.store.lock().await;
        store.add(job)
    }

    /// Remove a job.
    #[allow(dead_code)]
    pub async fn remove(&self, id: &str, channel: &str, user_id: &str) -> Result<Option<CronJob>> {
        let mut store = self.store.lock().await;
        store.remove(id, channel, user_id)
    }

    /// List jobs for a user.
    #[allow(dead_code)]
    pub async fn list(&self, channel: &str, user_id: &str) -> Vec<CronJob> {
        let store = self.store.lock().await;
        store
            .list_for_user(channel, user_id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Manually trigger a job (for testing via /cron run).
    #[allow(dead_code)]
    pub async fn run_now(
        &self,
        id: &str,
        channel: &str,
        user_id: &str,
        result_sender: ResultSender,
    ) -> Result<()> {
        let job = {
            let store = self.store.lock().await;
            store
                .get(id, channel, user_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Job not found: {}", id))?
        };

        let store = Arc::clone(&self.store);

        execute_job(job, store, result_sender, &self.clock).await;

        Ok(())
    }

    /// Get job status.
    #[allow(dead_code)]
    pub async fn status(&self, id: &str, channel: &str, user_id: &str) -> Option<CronJob> {
        let store = self.store.lock().await;
        store.get(id, channel, user_id).cloned()
    }

    /// Toggle job enabled state.
    #[allow(dead_code)]
    pub async fn toggle(&self, id: &str, channel: &str, user_id: &str) -> Result<bool> {
        let mut store = self.store.lock().await;

        // Verify ownership first
        let job = store
            .get(id, channel, user_id)
            .ok_or_else(|| anyhow::anyhow!("Job not found: {}", id))?;

        let new_state = !job.enabled;

        // Now update
        if let Some(job) = store.get_mut(id) {
            job.enabled = new_state;
            if new_state {
                job.update_next_run(self.clock.now_millis());
            } else {
                job.state.next_run_at = None;
            }
        }

        store.save()?;
        Ok(new_state)
    }
}

/// Execute a single job.
async fn execute_job<C: Clock>(
    job: CronJob,
    store: Arc<Mutex<CronStore>>,
    result_sender: ResultSender,
    clock: &C,
) {
    let job_id = job.id.clone();
    info!("Executing cron job: {} ({})", job.name, job.short_id());

    let start_time = clock.now_millis();

    // Mark as running and clear next_run_at to prevent duplicate execution
    {
        let mut store = store.lock().await;
        if let Some(job) = store.get_mut(&job_id) {
            job.state.last_status = JobStatus::Running;
            job.state.next_run_at = None; // Prevent re-triggering while running
        }
        let _ = store.save();
    }

    // Build context prompt so the job has access to skills, configs, etc.
    let channel_display = get_channel_info(&job.channel).map(|c| c.display_name);
    let context_prompt = onboarding::build_context_prompt_for_user(
        channel_display,
        Some(&job.channel),
        Some(&job.user_id),
        Some(&job.prompt),
    );

    // Execute the Claude prompt
    let result = match context_prompt {
        Ok(ctx) => {
            claude::query_with_options(
                &job.prompt,
                QueryOptions {
                    system_prompt: Some(ctx),
                    skip_permissions: true,
                    ..Default::default()
                },
            )
            .await
        }
        Err(e) => Err(e),
    };

    let end_time = clock.now_millis();
    let duration_ms = end_time - start_time;

    // Update job state
    {
        let mut store = store.lock().await;
        if let Some(stored_job) = store.get_mut(&job_id) {
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

            // Calculate next run time (for recurring jobs)
            stored_job.update_next_run(end_time);

            // For one-shot At jobs that have completed, disable them
            if matches!(stored_job.schedule, CronSchedule::At(_)) && result.is_ok() {
                stored_job.enabled = false;
                stored_job.state.next_run_at = None;
            }
        }
        let _ = store.save();
    }

    // Send result to user if notify is enabled
    if job.notify {
        let message = match result {
            Ok((response, _session_id)) => {
                format!("[Cron: {}]\n\n{}", job.name, response)
            }
            Err(e) => {
                format!("[Cron: {} FAILED]\n\nError: {}", job.name, e)
            }
        };

        if let Err(e) = result_sender(job.channel.clone(), job.user_id.clone(), message).await {
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
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
