/*
File: crates/ms-tab-translation/src/mt_limits/registry.rs

Purpose:
The process-wide holder of every machine-translation lane's limiter state (a lane is a
service over one transport, `table::MtLane`), and the clock seam it runs on. One registry (`MtLimiterRegistry::shared()`) is shared by the Translation tab's
MT runs (including a detached run that outlives Stop) and the Cleaning tab's prompt translator,
so two concurrent callers can never double a lane's request rate.

Key items:
- `MtClock` / `SystemClock`: now, sleep and jitter; tests inject a fake clock.
- `MtLimiterRegistry`: `Mutex<HashMap<MtLane, LaneState>>` + clock + limits table.
  `shared()`, `decide`, `finish`, `lane_cooldown`, `min_interval`, `clock`.

Notes:
- `SHARED` is a documented exception to global §5 ("no global mutable state"): the limiter
  models a property of the whole process (its request rate towards a remote service as seen by
  that service), so a per-owner instance would defeat its purpose. Precedents:
  `ms_download::busy::ACTIVE`, `ms_ai_api::neurolate` paid-flight cap.
- The lock is held for ONE pure decision (`limiter.rs`), never across a sleep, a callback or
  I/O. A poisoned lock is recovered (`PoisonError::into_inner`) and logged: every decision is a
  single update of plain counters that cannot be left half-applied by a panic elsewhere.
- Cooldowns live in memory only; a restart forgets them.
*/

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ms_log::runtime_log;
use web_time::Instant;

use super::call::MtSignal;
use super::limiter::{Decision, LaneState};
use super::table::{LimitsTable, MtLane, MtRoute};

/// Time source of the limiter. Production uses `SystemClock`; tests inject a fake whose
/// `sleep` advances `now`.
pub(crate) trait MtClock: Send + Sync {
    /// Monotonic now.
    fn now(&self) -> Instant;
    /// Blocks the calling (worker) thread for `duration`.
    fn sleep(&self, duration: Duration);
    /// A pseudo-random duration in the half-open range `[range.0, range.1)`; `range.0` when the
    /// range is empty.
    fn jitter(&self, range: (Duration, Duration)) -> Duration;
}

/// The real clock: `web_time::Instant`, `ms_thread::sleep`, jitter from the wall clock's
/// sub-second nanoseconds (the same weak randomness the DeepL backoff always used; it only
/// has to de-synchronise retries, not be unpredictable).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl MtClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        ms_thread::sleep(duration);
    }

    fn jitter(&self, range: (Duration, Duration)) -> Duration {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.subsec_nanos());
        jitter_in_range(range, nanos)
    }
}

/// Maps `seed` into `[range.0, range.1)` (`range.0` for an empty range).
fn jitter_in_range(range: (Duration, Duration), seed: u32) -> Duration {
    let (lo, hi) = range;
    let span_nanos = hi.saturating_sub(lo).as_nanos();
    if span_nanos == 0 {
        return lo;
    }
    // `seed % span` is below `span`, which came from a `Duration`; it fits in u64 because the
    // seed itself is a u32.
    let offset = u64::try_from(u128::from(seed) % span_nanos).unwrap_or(0);
    lo + Duration::from_nanos(offset)
}

/// The one process-wide registry (`MtLimiterRegistry::shared`).
static SHARED: OnceLock<MtLimiterRegistry> = OnceLock::new();

/// Limiter state of every machine-translation lane, shared by all callers in the process.
pub(crate) struct MtLimiterRegistry {
    clock: Arc<dyn MtClock>,
    table: LimitsTable,
    state: Mutex<HashMap<MtLane, LaneState>>,
}

impl std::fmt::Debug for MtLimiterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MtLimiterRegistry").field("table", &self.table).finish_non_exhaustive()
    }
}

impl MtLimiterRegistry {
    /// The process-wide registry over the system clock and the production table. Every
    /// production caller uses it; tests build their own with `new` / `with_table`.
    #[must_use]
    pub(crate) fn shared() -> &'static Self {
        SHARED.get_or_init(|| Self::with_table(Arc::new(SystemClock), LimitsTable::PRODUCTION))
    }

    /// A private registry on `clock` with the production numbers (tests of the limiter and of
    /// the providers).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(clock: Arc<dyn MtClock>) -> Self {
        Self::with_table(clock, LimitsTable::PRODUCTION)
    }

    /// A registry on `clock` and `table`.
    #[must_use]
    pub(crate) fn with_table(clock: Arc<dyn MtClock>, table: LimitsTable) -> Self {
        Self { clock, table, state: Mutex::new(HashMap::new()) }
    }

    /// The clock this registry runs on (callers sleep and read time through it).
    #[must_use]
    pub(crate) fn clock(&self) -> &dyn MtClock {
        self.clock.as_ref()
    }

    /// The ordinary start-to-start spacing of `lane` (`LaneLimits::min_interval`).
    #[must_use]
    pub(crate) fn min_interval(&self, lane: MtLane) -> Duration {
        (self.table.lane)(lane).min_interval
    }

    /// One admission decision for `route` (see `LaneState::decide`); holds the lock for
    /// the decision only.
    pub(crate) fn decide(&self, route: MtRoute, max_wait: Duration) -> Decision {
        let now = self.clock.now();
        let mut states = self.lock();
        self.state_of(&mut states, route.lane()).decide(route, now, max_wait)
    }

    /// Records the end of a request admitted on `route`; logs when the route starts cooling.
    pub(crate) fn finish(&self, route: MtRoute, signal: MtSignal) {
        // Jitter is drawn before locking: the clock is caller-supplied code.
        let jitter = self.clock.jitter((self.table.route)(route).jitter);
        let now = self.clock.now();
        let cooldown = {
            let mut states = self.lock();
            self.state_of(&mut states, route.lane()).finish(route, now, signal, jitter)
        };
        if let Some(cooldown) = cooldown {
            runtime_log::log_warn(format!(
                "[MT][limits] route {} is rate-limited: no requests for {:.1} s (signal {signal:?})",
                route.log_name(),
                cooldown.as_secs_f64()
            ));
        }
    }

    /// `Some(shortest remaining cooldown)` when every route of `lane` is cooling, i.e. the
    /// lane can send nothing right now; `None` otherwise.
    #[must_use]
    pub(crate) fn lane_cooldown(&self, lane: MtLane) -> Option<Duration> {
        let now = self.clock.now();
        let mut states = self.lock();
        self.state_of(&mut states, lane).lane_cooldown(now)
    }

    /// Whether the state lock is held right now (tests assert it is free during every sleep).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_locked(&self) -> bool {
        self.state.try_lock().is_err()
    }

    /// Locks the state map, recovering a poisoned lock (see the file header for why that is
    /// sound) with a log line.
    fn lock(&self) -> MutexGuard<'_, HashMap<MtLane, LaneState>> {
        self.state.lock().unwrap_or_else(|poisoned| {
            runtime_log::log_warn("[MT][limits] limiter state lock was poisoned by a panicking thread; continuing with the last consistent state");
            PoisonError::into_inner(poisoned)
        })
    }

    /// The state of `lane`, created on first use.
    fn state_of<'s>(&self, states: &'s mut HashMap<MtLane, LaneState>, lane: MtLane) -> &'s mut LaneState {
        states.entry(lane).or_insert_with(|| LaneState::new(lane, self.table))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_the_half_open_range() {
        let range = (Duration::from_millis(250), Duration::from_millis(750));
        for seed in [0, 1, 499_999_999, 500_000_000, u32::MAX] {
            let jitter = jitter_in_range(range, seed);
            assert!(jitter >= range.0 && jitter < range.1, "{jitter:?}");
        }
        assert_eq!(jitter_in_range((Duration::ZERO, Duration::ZERO), 77), Duration::ZERO);
        let system = SystemClock.jitter(range);
        assert!(system >= range.0 && system < range.1);
    }

    #[test]
    fn shared_registry_is_one_instance() {
        assert!(std::ptr::eq(MtLimiterRegistry::shared(), MtLimiterRegistry::shared()));
    }
}
