/*
FILE OVERVIEW: crates/ms-models/src/autosave_gate.rs
The shared "is an autosave flush due?" decision for every background writer of a
project instance (layer saver, bubbles saver, clean-overlay autosave).

Writers HOLD their pending work in memory and write it to the chapter's `_unsaved`
staging folder only when the gate's flush epoch moves. The epoch moves when:
- the autosave interval has elapsed since the FIRST pending action of the window, or
- the number of actions in the window reaches the action threshold, or
- `force_flush` is called (save-to-project completion, page operations).

An "action" is one enqueued save GESTURE (a stroke commit, a text edit, a bubble
change) reported through `note_action` — never a per-frame pixel mark.

The policy is read from `ms_config::autosave_policy::autosave_policy()` on every call
(or from an injected source for tests), so a settings change propagates live with no
channel to the workers. Time is `web_time::Instant` so the gate also builds for wasm.

Key types:
- `AutosaveGate`

Key functions:
- `AutosaveGate::new` / `AutosaveGate::with_policy_fn`
- `note_action` / `poll` / `wait_deadline` / `force_flush`
- `action_count`: the monotonic lifetime action counter (single-image dirty tracking)
*/

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ms_config::autosave_policy::{AutosavePolicy, autosave_policy};
use web_time::Instant;

/// Source of the policy consulted on every gate call.
type PolicyFn = Box<dyn Fn() -> AutosavePolicy + Send + Sync>;

/// Mutable window state of the gate.
#[derive(Debug, Default)]
struct GateState {
    /// Time of the first action of the current window; `None` = nothing pending.
    first_pending_at: Option<Instant>,
    /// Actions noted in the current window.
    actions: u32,
    /// Monotonic flush counter; each increment means "flush everything held now".
    flush_epoch: u64,
    /// Lifetime count of noted actions; never reset by a window close.
    total_actions: u64,
}

impl GateState {
    /// Closes the current window: bumps the epoch and forgets the pending actions.
    fn advance(&mut self) {
        self.flush_epoch = self.flush_epoch.wrapping_add(1);
        self.actions = 0;
        self.first_pending_at = None;
    }
}

/// Per-project autosave flush gate, shared (`Arc`) by all background writers of one
/// project instance.
///
/// Each writer remembers the last epoch it flushed at (`seen_epoch`); a flush is due for
/// it exactly when `poll() != seen_epoch`. The epoch only increases (a `u64` cannot wrap
/// in practice). Every method takes a short internal lock and does no I/O, so it is
/// safe to call from the GUI thread.
pub struct AutosaveGate {
    inner: Mutex<GateState>,
    policy: PolicyFn,
}

impl fmt::Debug for AutosaveGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutosaveGate").field("state", &*self.lock()).finish_non_exhaustive()
    }
}

impl Default for AutosaveGate {
    fn default() -> Self {
        Self::new()
    }
}

impl AutosaveGate {
    /// Creates a gate that reads the process-global policy
    /// (`ms_config::autosave_policy::autosave_policy`) on every call.
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy_fn(autosave_policy)
    }

    /// Creates a gate with an explicit policy source, consulted on every call. Meant for
    /// tests (e.g. a tiny interval) that must not touch — and race on — the process
    /// globals.
    #[must_use]
    pub fn with_policy_fn(policy: impl Fn() -> AutosavePolicy + Send + Sync + 'static) -> Self {
        Self { inner: Mutex::new(GateState::default()), policy: Box::new(policy) }
    }

    /// Records one save gesture. Opens the window on the first action; closes it (bumps
    /// the epoch) when the window's action count reaches the policy's threshold (a
    /// threshold of `0` behaves like `1`).
    pub fn note_action(&self) {
        self.note_action_at(Instant::now());
    }

    /// Returns the current flush epoch, first closing the window if the interval has
    /// elapsed since its first action.
    #[must_use]
    pub fn poll(&self) -> u64 {
        self.poll_at(Instant::now())
    }

    /// Time until the current window's interval elapses: `None` when nothing is pending,
    /// `Some(Duration::ZERO)` when a flush is already overdue (call `poll`). Writers use it
    /// to bound their channel wait.
    #[must_use]
    pub fn wait_deadline(&self) -> Option<Duration> {
        self.wait_deadline_at(Instant::now())
    }

    /// Closes the window unconditionally and returns the new epoch. Used when everything
    /// has just been committed (save-to-project) or is being forced out (page operations).
    pub fn force_flush(&self) -> u64 {
        let mut state = self.lock();
        state.advance();
        state.flush_epoch
    }

    /// Total number of actions noted over the gate's lifetime. Monotonic: it never decreases
    /// and is NOT reset when a window closes (unlike the per-window count). Single-image mode
    /// compares it against the value captured at its last successful file write to decide
    /// whether the session is dirty (`dev-docs/single_image_mode_plan.md` D6).
    #[must_use]
    pub fn action_count(&self) -> u64 {
        self.lock().total_actions
    }

    /// `note_action` at an explicit instant (deterministic tests).
    fn note_action_at(&self, now: Instant) {
        let policy = (self.policy)();
        let mut state = self.lock();
        state.total_actions = state.total_actions.saturating_add(1);
        if state.first_pending_at.is_none() {
            state.first_pending_at = Some(now);
        }
        state.actions = state.actions.saturating_add(1);
        if state.actions >= policy.action_threshold.max(1) {
            state.advance();
        }
    }

    /// `poll` at an explicit instant (deterministic tests).
    fn poll_at(&self, now: Instant) -> u64 {
        let policy = (self.policy)();
        let mut state = self.lock();
        let elapsed = state.first_pending_at.is_some_and(|first| now.saturating_duration_since(first) >= policy.interval);
        if elapsed {
            state.advance();
        }
        state.flush_epoch
    }

    /// `wait_deadline` at an explicit instant (deterministic tests).
    fn wait_deadline_at(&self, now: Instant) -> Option<Duration> {
        let policy = (self.policy)();
        let state = self.lock();
        state.first_pending_at.map(|first| policy.interval.saturating_sub(now.saturating_duration_since(first)))
    }

    /// Locks the state. A poisoned lock is recovered: the state is plain counters
    /// updated atomically under the lock, so no invariant can be half-applied by a panic.
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(interval: Duration, action_threshold: u32) -> AutosaveGate {
        AutosaveGate::with_policy_fn(move || AutosavePolicy { interval, action_threshold })
    }

    #[test]
    fn threshold_trips_at_exactly_n() {
        let gate = gate(Duration::from_secs(3600), 3);
        let t0 = Instant::now();
        gate.note_action_at(t0);
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 0);
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 1);
        // The count restarted: two more actions do not trip again.
        gate.note_action_at(t0);
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 1);
    }

    #[test]
    fn threshold_one_flushes_every_action_and_zero_behaves_like_one() {
        for threshold in [0, 1] {
            let gate = gate(Duration::from_secs(3600), threshold);
            let t0 = Instant::now();
            gate.note_action_at(t0);
            assert_eq!(gate.poll_at(t0), 1);
            gate.note_action_at(t0);
            assert_eq!(gate.poll_at(t0), 2);
        }
    }

    #[test]
    fn interval_counts_from_first_action_not_last() {
        let interval = Duration::from_secs(60);
        let gate = gate(interval, 1000);
        let t0 = Instant::now();
        gate.note_action_at(t0);
        gate.note_action_at(t0 + Duration::from_secs(50));
        assert_eq!(gate.poll_at(t0 + Duration::from_secs(59)), 0);
        assert_eq!(gate.wait_deadline_at(t0 + Duration::from_secs(50)), Some(Duration::from_secs(10)));
        // 60 s after the first action, only 10 s after the last one: due.
        assert_eq!(gate.poll_at(t0 + interval), 1);
    }

    #[test]
    fn advance_resets_window_and_count() {
        let gate = gate(Duration::from_secs(60), 3);
        let t0 = Instant::now();
        gate.note_action_at(t0);
        gate.note_action_at(t0);
        assert_eq!(gate.force_flush(), 1);
        assert_eq!(gate.wait_deadline_at(t0), None);
        // Count restarted: two actions after the flush do not reach the threshold of 3.
        let t1 = t0 + Duration::from_secs(100);
        gate.note_action_at(t1);
        gate.note_action_at(t1);
        assert_eq!(gate.poll_at(t1), 1);
        // The window restarted at `t1`, not at `t0`.
        assert_eq!(gate.poll_at(t1 + Duration::from_secs(59)), 1);
        assert_eq!(gate.poll_at(t1 + Duration::from_secs(60)), 2);
    }

    #[test]
    fn epochs_are_monotonic_and_idle_poll_does_not_advance() {
        let gate = gate(Duration::from_secs(1), 2);
        let t0 = Instant::now();
        let mut last = gate.poll_at(t0);
        for step in 0..10_u64 {
            let now = t0 + Duration::from_secs(step * 5);
            gate.note_action_at(now);
            let epoch = if step % 2 == 0 { gate.poll_at(now + Duration::from_secs(2)) } else { gate.force_flush() };
            assert!(epoch > last, "epoch must strictly increase after a flush");
            last = epoch;
            // Idle: repeated polls long after do not advance without new actions.
            assert_eq!(gate.poll_at(now + Duration::from_secs(4)), last);
        }
    }

    #[test]
    fn wait_deadline_none_when_idle_zero_when_overdue() {
        let gate = gate(Duration::from_secs(60), 100);
        let t0 = Instant::now();
        assert_eq!(gate.wait_deadline_at(t0), None);
        gate.note_action_at(t0);
        assert_eq!(gate.wait_deadline_at(t0 + Duration::from_secs(90)), Some(Duration::ZERO));
    }

    #[test]
    fn policy_is_read_live_on_every_call() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let threshold = Arc::new(AtomicU32::new(100));
        let source = Arc::clone(&threshold);
        let gate = AutosaveGate::with_policy_fn(move || AutosavePolicy { interval: Duration::from_secs(3600), action_threshold: source.load(Ordering::Relaxed) });
        let t0 = Instant::now();
        gate.note_action_at(t0);
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 0);
        threshold.store(3, Ordering::Relaxed);
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 1);
    }

    #[test]
    fn action_count_is_monotonic_across_window_closes() {
        let gate = gate(Duration::from_secs(60), 2);
        let t0 = Instant::now();
        assert_eq!(gate.action_count(), 0);
        gate.note_action_at(t0);
        assert_eq!(gate.action_count(), 1);
        // The threshold closes the window; the lifetime count keeps going.
        gate.note_action_at(t0);
        assert_eq!(gate.poll_at(t0), 1);
        assert_eq!(gate.action_count(), 2);
        gate.force_flush();
        assert_eq!(gate.action_count(), 2);
        gate.note_action_at(t0 + Duration::from_secs(100));
        assert_eq!(gate.poll_at(t0 + Duration::from_secs(200)), 3);
        assert_eq!(gate.action_count(), 3);
    }

    #[test]
    fn public_wall_clock_api_is_consistent() {
        let gate = gate(Duration::from_secs(3600), 2);
        assert_eq!(gate.wait_deadline(), None);
        gate.note_action();
        assert!(gate.wait_deadline().is_some());
        assert_eq!(gate.poll(), 0);
        gate.note_action();
        assert_eq!(gate.poll(), 1);
    }
}
