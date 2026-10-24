/*
File: crates/ms-tab-translation/src/mt_limits/test_support.rs

Purpose:
Test-only fake clock for the limiter and for the providers' tests: `sleep` advances `now`
instantly, every sleep is recorded, the jitter is injected, and every sleep asserts that the
watched registry's lock is free (the "never sleep under the lock" contract).

Key items:
- `FakeClock`, `FakeClock::registry()` / `registry_with_jitter()` (a fresh production-table
  registry already watched by its clock), `sleeps()`, `advance()`.
*/

use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use web_time::Instant;

use super::registry::{MtClock, MtLimiterRegistry};

/// A manual clock: time moves only through `sleep` and `advance`.
#[derive(Debug)]
pub(crate) struct FakeClock {
    now: Mutex<Instant>,
    sleeps: Mutex<Vec<Duration>>,
    jitter: Duration,
    watched: OnceLock<Weak<MtLimiterRegistry>>,
}

impl FakeClock {
    /// A fresh production-table registry on a fake clock without jitter.
    #[must_use]
    pub(crate) fn registry() -> (Arc<Self>, Arc<MtLimiterRegistry>) {
        Self::registry_with_jitter(Duration::ZERO)
    }

    /// A fresh production-table registry on a fake clock whose `jitter` answers `jitter` for
    /// every non-empty jitter range (empty ranges still give their lower bound).
    #[must_use]
    pub(crate) fn registry_with_jitter(jitter: Duration) -> (Arc<Self>, Arc<MtLimiterRegistry>) {
        let clock = Arc::new(Self { now: Mutex::new(Instant::now()), sleeps: Mutex::new(Vec::new()), jitter, watched: OnceLock::new() });
        let registry = Arc::new(MtLimiterRegistry::new(clock.clone()));
        clock.watched.get_or_init(|| Arc::downgrade(&registry));
        (clock, registry)
    }

    /// Every sleep so far, in order.
    #[must_use]
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Moves time forward without recording a sleep (time passing between test steps).
    pub(crate) fn advance(&self, duration: Duration) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) += duration;
    }
}

impl MtClock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep(&self, duration: Duration) {
        if let Some(registry) = self.watched.get().and_then(Weak::upgrade) {
            assert!(!registry.is_locked(), "the limiter slept while holding its state lock");
        }
        self.sleeps.lock().unwrap_or_else(PoisonError::into_inner).push(duration);
        self.advance(duration);
    }

    fn jitter(&self, range: (Duration, Duration)) -> Duration {
        if range.1 > range.0 { self.jitter } else { range.0 }
    }
}
