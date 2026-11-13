//! Injectable time source.
//!
//! Everything that reads the time (state timestamps, `time_fired`, timers and,
//! later, sun maths) goes through a [`Clock`], so tests can run on simulated
//! time instead of sleeping.
//!
//! Monotonic time is always [`tokio::time::Instant`]: under
//! `tokio::time::pause()` it only moves when a test calls `advance`, and
//! `sleep_until` timers fire on that simulated clock. A [`SimulatedClock`]
//! derives its wall time from the same monotonic clock, so advancing tokio's
//! time moves both together, while [`SimulatedClock::jump_to`] moves the wall
//! clock alone (an NTP step or a DST test).

use jiff::{SignedDuration, Timestamp};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::time::Instant;

/// A wall + monotonic clock. Cheap to clone; the system clock is the default.
#[derive(Clone, Default)]
pub struct Clock(Source);

#[derive(Clone, Default)]
enum Source {
    #[default]
    System,
    Simulated(Arc<Mutex<Anchor>>),
}

/// The wall time at a given monotonic instant.
#[derive(Clone, Copy)]
struct Anchor {
    wall: Timestamp,
    at: Instant,
}

impl Clock {
    /// The real system clock.
    pub fn system() -> Self {
        Self(Source::System)
    }

    /// Current wall-clock time.
    pub fn now(&self) -> Timestamp {
        match &self.0 {
            Source::System => Timestamp::now(),
            Source::Simulated(anchor) => anchor.lock().wall_at(Instant::now()),
        }
    }

    /// Current monotonic time. Use it for durations and `sleep_until`
    /// deadlines, never the wall clock.
    pub fn instant(&self) -> Instant {
        Instant::now()
    }
}

impl Anchor {
    fn wall_at(&self, now: Instant) -> Timestamp {
        // `Instant` arithmetic saturates and `SignedDuration` covers ±292
        // years, so neither step can overflow in practice.
        let elapsed = SignedDuration::try_from(now.saturating_duration_since(self.at))
            .unwrap_or(SignedDuration::MAX);
        self.wall.saturating_add(elapsed).unwrap_or(self.wall)
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Source::System => f.write_str("Clock::System"),
            Source::Simulated(_) => write!(f, "Clock::Simulated({})", self.now()),
        }
    }
}

/// Test handle for a simulated clock: its wall time starts at `start` and
/// then follows tokio's (pausable) monotonic clock.
#[derive(Clone)]
pub struct SimulatedClock {
    anchor: Arc<Mutex<Anchor>>,
}

impl SimulatedClock {
    pub fn new(start: Timestamp) -> Self {
        Self {
            anchor: Arc::new(Mutex::new(Anchor {
                wall: start,
                at: Instant::now(),
            })),
        }
    }

    /// The [`Clock`] to hand to [`crate::Hass::with_clock`].
    pub fn clock(&self) -> Clock {
        Clock(Source::Simulated(self.anchor.clone()))
    }

    /// Step the wall clock to `wall` without moving monotonic time, like an
    /// NTP correction. Pending `sleep_until` timers are unaffected.
    pub fn jump_to(&self, wall: Timestamp) {
        *self.anchor.lock() = Anchor {
            wall,
            at: Instant::now(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn simulated_wall_time_follows_tokio_time() {
        let sim = SimulatedClock::new(ts("2026-03-29T00:59:00Z"));
        let clock = sim.clock();
        let start = clock.instant();
        assert_eq!(clock.now(), ts("2026-03-29T00:59:00Z"));

        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(clock.now(), ts("2026-03-29T01:00:30Z"));
        assert_eq!(clock.instant() - start, Duration::from_secs(90));
    }

    #[tokio::test(start_paused = true)]
    async fn timers_fire_on_simulated_time() {
        let clock = SimulatedClock::new(ts("2026-01-01T00:00:00Z")).clock();
        let deadline = clock.instant() + Duration::from_secs(3600);
        // Paused tokio auto-advances to the deadline instead of waiting an hour.
        tokio::time::sleep_until(deadline).await;
        assert_eq!(clock.now(), ts("2026-01-01T01:00:00Z"));
    }

    #[tokio::test(start_paused = true)]
    async fn jump_moves_wall_time_only() {
        let sim = SimulatedClock::new(ts("2026-01-01T00:00:00Z"));
        let clock = sim.clock();
        let start = clock.instant();
        sim.jump_to(ts("2025-12-31T23:00:00Z"));
        assert_eq!(clock.now(), ts("2025-12-31T23:00:00Z"));
        assert_eq!(clock.instant(), start, "monotonic time did not move");

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(clock.now(), ts("2025-12-31T23:00:01Z"));
    }

    #[test]
    fn system_clock_is_the_default() {
        let before = Timestamp::now();
        let now = Clock::default().now();
        assert!(now >= before && now <= Timestamp::now());
    }
}
