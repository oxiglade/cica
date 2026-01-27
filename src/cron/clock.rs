//! Clock abstraction for testable time handling.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Clock trait for abstracting time - enables testing without real timers.
pub trait Clock: Send + Sync + Clone + 'static {
    /// Get current time in milliseconds since Unix epoch.
    fn now_millis(&self) -> u64;

    /// Sleep for a duration.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Real system clock for production use.
#[derive(Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Fake clock for testing - time can be manually advanced.
#[derive(Clone)]
#[allow(dead_code)]
pub struct FakeClock {
    current_time: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl FakeClock {
    /// Create a new fake clock starting at the given time.
    pub fn new(initial_time_ms: u64) -> Self {
        Self {
            current_time: Arc::new(AtomicU64::new(initial_time_ms)),
        }
    }

    /// Advance time by the specified duration in milliseconds.
    pub fn advance_ms(&self, duration_ms: u64) {
        self.current_time.fetch_add(duration_ms, Ordering::SeqCst);
    }

    /// Advance time by a Duration.
    pub fn advance(&self, duration: Duration) {
        self.advance_ms(duration.as_millis() as u64);
    }

    /// Set time to a specific value.
    pub fn set(&self, time_ms: u64) {
        self.current_time.store(time_ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> u64 {
        self.current_time.load(Ordering::SeqCst)
    }

    fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // In tests, sleep is instant - time is controlled manually via advance()
        Box::pin(async { tokio::task::yield_now().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_clock() {
        let clock = SystemClock;
        let now = clock.now_millis();
        assert!(now > 0);

        // Time should advance
        std::thread::sleep(Duration::from_millis(10));
        assert!(clock.now_millis() > now);
    }

    #[test]
    fn test_fake_clock() {
        let clock = FakeClock::new(1000);
        assert_eq!(clock.now_millis(), 1000);

        clock.advance_ms(500);
        assert_eq!(clock.now_millis(), 1500);

        clock.set(5000);
        assert_eq!(clock.now_millis(), 5000);
    }

    #[test]
    fn test_fake_clock_clone() {
        let clock1 = FakeClock::new(1000);
        let clock2 = clock1.clone();

        clock1.advance_ms(500);

        // Both clones should see the same time (Arc shared state)
        assert_eq!(clock1.now_millis(), 1500);
        assert_eq!(clock2.now_millis(), 1500);
    }
}
