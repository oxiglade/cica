//! Schedule types and parsing for cron jobs.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use croner::Cron;
use serde::{Deserialize, Serialize};

/// Supported schedule types for cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value")]
pub enum CronSchedule {
    /// One-shot execution at a specific timestamp (Unix milliseconds).
    /// Example: "at 2024-01-28 14:00"
    At(u64),

    /// Recurring interval in milliseconds.
    /// Example: "every 1h", "every 10s"
    Every(u64),

    /// Standard cron expression.
    /// Example: "0 9 * * *" (9 AM daily)
    Cron(String),
}

impl CronSchedule {
    /// Parse a schedule string into a CronSchedule.
    ///
    /// Formats:
    /// - "at 2024-01-28 14:00" or "at 2024-01-28T14:00:00"
    /// - "every 10s", "every 5m", "every 1h", "every 2d"
    /// - "0 9 * * *" (cron expression - 5 fields)
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();

        if input.starts_with("at ") {
            let datetime_str = input.strip_prefix("at ").unwrap().trim();
            let timestamp_ms = parse_datetime(datetime_str)?;
            return Ok(CronSchedule::At(timestamp_ms));
        }

        if input.starts_with("every ") {
            let interval_str = input.strip_prefix("every ").unwrap().trim();
            let duration_ms = parse_duration(interval_str)?;
            return Ok(CronSchedule::Every(duration_ms));
        }

        // Assume cron expression - validate it
        validate_cron_expression(input)?;
        Ok(CronSchedule::Cron(input.to_string()))
    }

    /// Calculate next run time from a reference timestamp (in milliseconds).
    /// Returns None if the schedule has no future runs (e.g., one-shot in the past).
    pub fn next_run_after(&self, after_ms: u64) -> Option<u64> {
        match self {
            CronSchedule::At(ts) => {
                if *ts > after_ms {
                    Some(*ts)
                } else {
                    None
                }
            }
            CronSchedule::Every(interval) => Some(after_ms + interval),
            CronSchedule::Cron(expr) => calculate_next_cron(expr, after_ms),
        }
    }

    /// Human-readable description of the schedule.
    pub fn description(&self) -> String {
        match self {
            CronSchedule::At(ts) => {
                let dt = DateTime::from_timestamp_millis(*ts as i64)
                    .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| ts.to_string());
                format!("at {}", dt)
            }
            CronSchedule::Every(ms) => format_duration(*ms),
            CronSchedule::Cron(expr) => expr.clone(),
        }
    }
}

/// Parse duration strings like "10s", "5m", "1h", "2d".
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Empty duration string".to_string());
    }

    // Find where digits end and unit begins
    let num_end = s
        .chars()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(s.len());

    if num_end == 0 {
        return Err(format!("Invalid duration: {}", s));
    }

    let (num_str, unit) = s.split_at(num_end);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("Invalid number: {}", num_str))?;

    let unit = unit.trim();
    let multiplier = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        _ => return Err(format!("Invalid unit: {}. Use s/m/h/d", unit)),
    };

    Ok(num * multiplier)
}

/// Format milliseconds as a human-readable duration.
fn format_duration(ms: u64) -> String {
    if ms >= 86_400_000 && ms.is_multiple_of(86_400_000) {
        format!("every {}d", ms / 86_400_000)
    } else if ms >= 3_600_000 && ms.is_multiple_of(3_600_000) {
        format!("every {}h", ms / 3_600_000)
    } else if ms >= 60_000 && ms.is_multiple_of(60_000) {
        format!("every {}m", ms / 60_000)
    } else if ms >= 1_000 && ms.is_multiple_of(1_000) {
        format!("every {}s", ms / 1_000)
    } else {
        format!("every {}ms", ms)
    }
}

/// Parse datetime string into Unix milliseconds.
/// Supports: "2024-01-28 14:00", "2024-01-28T14:00:00", etc.
fn parse_datetime(s: &str) -> Result<u64, String> {
    let s = s.trim();

    // Try various formats
    let formats = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ];

    for fmt in &formats {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            let local = Local.from_local_datetime(&naive).single();
            if let Some(dt) = local {
                return Ok(dt.timestamp_millis() as u64);
            }
        }
    }

    Err(format!(
        "Invalid datetime: {}. Use format: YYYY-MM-DD HH:MM",
        s
    ))
}

/// Validate a cron expression.
fn validate_cron_expression(expr: &str) -> Result<(), String> {
    Cron::new(expr)
        .parse()
        .map_err(|e| format!("Invalid cron expression: {}", e))?;
    Ok(())
}

/// Calculate next run time for a cron expression.
fn calculate_next_cron(expr: &str, after_ms: u64) -> Option<u64> {
    let cron = Cron::new(expr).parse().ok()?;
    let after = DateTime::from_timestamp_millis(after_ms as i64)?;
    let next = cron.find_next_occurrence(&after, false).ok()?;
    Some(next.timestamp_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("10s").unwrap(), 10_000);
        assert_eq!(parse_duration("5m").unwrap(), 300_000);
        assert_eq!(parse_duration("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration("2d").unwrap(), 172_800_000);
        assert_eq!(parse_duration("30min").unwrap(), 1_800_000);
        assert_eq!(parse_duration("24hours").unwrap(), 86_400_000);
    }

    #[test]
    fn test_parse_duration_errors() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
    }

    #[test]
    fn test_schedule_parse_every() {
        assert!(matches!(
            CronSchedule::parse("every 10s"),
            Ok(CronSchedule::Every(10_000))
        ));
        assert!(matches!(
            CronSchedule::parse("every 1h"),
            Ok(CronSchedule::Every(3_600_000))
        ));
    }

    #[test]
    fn test_schedule_parse_cron() {
        let result = CronSchedule::parse("0 9 * * *");
        assert!(matches!(result, Ok(CronSchedule::Cron(_))));
    }

    #[test]
    fn test_schedule_next_run_every() {
        let schedule = CronSchedule::Every(60_000);
        assert_eq!(schedule.next_run_after(1000), Some(61_000));
    }

    #[test]
    fn test_schedule_next_run_at() {
        let schedule = CronSchedule::At(5000);
        assert_eq!(schedule.next_run_after(1000), Some(5000));
        assert_eq!(schedule.next_run_after(5000), None);
        assert_eq!(schedule.next_run_after(6000), None);
    }
}
