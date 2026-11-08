//! Clock abstraction for alarms.
//! Enables deterministic testing of time jumps, countdowns, and schedule triggering.

use std::sync::{Arc, Mutex};
use std::time::Instant;
use time::OffsetDateTime;

/// Abstract clock source
pub trait Clock: Send + Sync + 'static {
    /// Return current wall-clock calendar time in UTC
    fn now_utc(&self) -> OffsetDateTime;

    /// Return current monotonic instant (for measuring elapsed durations without clock skew)
    fn now_monotonic(&self) -> Instant;
}

/// Standard production system clock
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn now_monotonic(&self) -> Instant {
        Instant::now()
    }
}

/// Controllable mock clock for deterministic unit and integration tests
#[derive(Debug, Clone)]
pub struct TestClock {
    state: Arc<Mutex<TestClockState>>,
}

#[derive(Debug)]
struct TestClockState {
    now_utc: OffsetDateTime,
    now_monotonic: Instant,
}

impl TestClock {
    pub fn new(initial_utc: OffsetDateTime) -> Self {
        Self {
            state: Arc::new(Mutex::new(TestClockState {
                now_utc: initial_utc,
                now_monotonic: Instant::now(),
            })),
        }
    }

    /// Advance both wall-clock and monotonic time by a duration
    pub fn advance(&self, duration: std::time::Duration) {
        let mut state = self.state.lock().unwrap();
        state.now_utc += time::Duration::try_from(duration).unwrap_or(time::Duration::ZERO);
        state.now_monotonic += duration;
    }

    /// Simulate a sudden clock step / jump (e.g. NTP sync after network recovery)
    /// Wall-clock changes, but monotonic time remains smooth.
    pub fn step_wall_clock(&self, new_utc: OffsetDateTime) {
        let mut state = self.state.lock().unwrap();
        state.now_utc = new_utc;
    }
}

impl Clock for TestClock {
    fn now_utc(&self) -> OffsetDateTime {
        self.state.lock().unwrap().now_utc
    }

    fn now_monotonic(&self) -> Instant {
        self.state.lock().unwrap().now_monotonic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use time::macros::datetime;

    #[test]
    fn test_test_clock_advance() {
        let start = datetime!(2026-11-05 07:00:00 UTC);
        let clock = TestClock::new(start);
        assert_eq!(clock.now_utc(), start);

        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.now_utc(), datetime!(2026-11-05 07:01:00 UTC));
    }

    #[test]
    fn test_test_clock_step_wall_clock() {
        let start = datetime!(2026-11-05 07:00:00 UTC);
        let clock = TestClock::new(start);

        clock.step_wall_clock(datetime!(2026-11-05 12:00:00 UTC));
        assert_eq!(clock.now_utc(), datetime!(2026-11-05 12:00:00 UTC));
    }
}
