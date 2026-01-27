//! Persistent storage for cron jobs.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

use super::schedule::CronSchedule;

/// Unique identifier for a cron job.
pub type JobId = String;

/// Status of last job execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(tag = "status", content = "error")]
pub enum JobStatus {
    #[default]
    Pending,
    Running,
    Success,
    Failed(String),
}

impl JobStatus {
    pub fn as_str(&self) -> &str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Success => "success",
            JobStatus::Failed(_) => "failed",
        }
    }
}

/// Runtime state for a job (mutable between runs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronJobState {
    /// Next scheduled run time (Unix millis).
    pub next_run_at: Option<u64>,

    /// Last run timestamp (Unix millis).
    pub last_run_at: Option<u64>,

    /// Status of last execution.
    #[serde(default)]
    pub last_status: JobStatus,

    /// Last execution duration in milliseconds.
    pub last_duration_ms: Option<u64>,

    /// Count of consecutive failures.
    #[serde(default)]
    pub failure_count: u32,
}

/// A scheduled cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique job ID.
    pub id: JobId,

    /// Human-readable name.
    pub name: String,

    /// The Claude Code prompt to execute.
    pub prompt: String,

    /// Schedule configuration.
    pub schedule: CronSchedule,

    /// Owner: channel name (e.g., "telegram", "signal").
    pub channel: String,

    /// Owner: user ID within the channel.
    pub user_id: String,

    /// Whether to send results back to the user's chat.
    #[serde(default = "default_true")]
    pub notify: bool,

    /// Job is enabled (can be paused).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Creation timestamp (Unix millis).
    pub created_at: u64,

    /// Runtime state.
    #[serde(default)]
    pub state: CronJobState,
}

fn default_true() -> bool {
    true
}

impl CronJob {
    /// Create a new job with generated ID.
    pub fn new(
        name: String,
        prompt: String,
        schedule: CronSchedule,
        channel: String,
        user_id: String,
    ) -> Self {
        let now = now_millis();
        let mut job = Self {
            id: generate_job_id(),
            name,
            prompt,
            schedule,
            channel,
            user_id,
            notify: true,
            enabled: true,
            created_at: now,
            state: CronJobState::default(),
        };
        job.update_next_run(now);
        job
    }

    /// User key for ownership (channel:user_id).
    #[allow(dead_code)]
    pub fn user_key(&self) -> String {
        format!("{}:{}", self.channel, self.user_id)
    }

    /// Calculate and update next_run_at based on given time.
    pub fn update_next_run(&mut self, now_ms: u64) {
        self.state.next_run_at = self.schedule.next_run_after(now_ms);
    }

    /// Check if this job is due to run.
    pub fn is_due(&self, now_ms: u64) -> bool {
        self.enabled && self.state.next_run_at.is_some_and(|t| t <= now_ms)
    }

    /// Short ID for display (first 8 chars).
    pub fn short_id(&self) -> &str {
        if self.id.len() > 8 {
            &self.id[..8]
        } else {
            &self.id
        }
    }
}

/// Persistent storage for cron jobs.
/// Follows PairingStore pattern with JSON file persistence.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronStore {
    /// All jobs indexed by ID.
    pub jobs: HashMap<JobId, CronJob>,
}

impl CronStore {
    /// Load cron store from disk.
    pub fn load() -> Result<Self> {
        let paths = config::paths()?;
        let path = paths.base.join("cron.json");

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read cron file: {:?}", path))?;

        let store: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse cron file: {:?}", path))?;

        Ok(store)
    }

    /// Save cron store to disk.
    pub fn save(&self) -> Result<()> {
        let paths = config::paths()?;
        let path = paths.base.join("cron.json");

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;

        Ok(())
    }

    /// Add a new job.
    pub fn add(&mut self, job: CronJob) -> Result<JobId> {
        let id = job.id.clone();
        self.jobs.insert(id.clone(), job);
        self.save()?;

        Ok(id)
    }

    /// Remove a job by ID (only if user owns it).
    pub fn remove(&mut self, id: &str, channel: &str, user_id: &str) -> Result<Option<CronJob>> {
        // Check ownership first
        if let Some(job) = self.jobs.get(id)
            && (job.channel != channel || job.user_id != user_id)
        {
            anyhow::bail!("You don't own this job");
        }

        let removed = self.jobs.remove(id);
        if removed.is_some() {
            self.save()?;
        }

        Ok(removed)
    }

    /// List jobs for a specific user.
    pub fn list_for_user(&self, channel: &str, user_id: &str) -> Vec<&CronJob> {
        self.jobs
            .values()
            .filter(|j| j.channel == channel && j.user_id == user_id)
            .collect()
    }

    /// Get a job by ID (with ownership check).
    pub fn get(&self, id: &str, channel: &str, user_id: &str) -> Option<&CronJob> {
        self.jobs
            .get(id)
            .filter(|j| j.channel == channel && j.user_id == user_id)
    }

    /// Get mutable reference (internal use, no ownership check).
    pub fn get_mut(&mut self, id: &str) -> Option<&mut CronJob> {
        self.jobs.get_mut(id)
    }

    /// Get all jobs that are due to run.
    pub fn get_due_jobs(&self, now_ms: u64) -> Vec<&CronJob> {
        self.jobs.values().filter(|j| j.is_due(now_ms)).collect()
    }

    /// Get all enabled jobs (for scheduler).
    #[allow(dead_code)]
    pub fn get_enabled_jobs(&self) -> Vec<&CronJob> {
        self.jobs.values().filter(|j| j.enabled).collect()
    }
}

/// Generate a unique job ID.
fn generate_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Get current time in milliseconds.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_creation() {
        let job = CronJob::new(
            "Test Job".to_string(),
            "Test prompt".to_string(),
            CronSchedule::Every(60_000),
            "telegram".to_string(),
            "12345".to_string(),
        );

        assert!(!job.id.is_empty());
        assert_eq!(job.name, "Test Job");
        assert_eq!(job.channel, "telegram");
        assert!(job.enabled);
        assert!(job.notify);
        assert!(job.state.next_run_at.is_some());
    }

    #[test]
    fn test_job_due_check() {
        let mut job = CronJob::new(
            "Test".to_string(),
            "Test".to_string(),
            CronSchedule::Every(60_000),
            "test".to_string(),
            "user1".to_string(),
        );

        // Set next_run to 1000
        job.state.next_run_at = Some(1000);

        assert!(!job.is_due(500)); // Before
        assert!(job.is_due(1000)); // Exact
        assert!(job.is_due(1500)); // After

        // Disabled job should never be due
        job.enabled = false;
        assert!(!job.is_due(1500));
    }

    #[test]
    fn test_user_key() {
        let job = CronJob::new(
            "Test".to_string(),
            "Test".to_string(),
            CronSchedule::Every(60_000),
            "telegram".to_string(),
            "12345".to_string(),
        );

        assert_eq!(job.user_key(), "telegram:12345");
    }
}
