/*
FILE OVERVIEW: crates/ms-config/src/autosave_policy.rs
App-wide runtime autosave policy: how long pending edits may stay in memory before the
background writers flush them to the chapter's `_unsaved` staging folder, and how many
save gestures ("actions") force a flush earlier.

Like `rotation_ctrl_wheel.rs`, this module owns only the thread-safe runtime global; it
reads no config document itself. The app seeds it at startup from `General`
(`seed_autosave_policy_from_user_settings`), the shared general-settings pane edits it
live through the `set_*` functions, and `ms_models::autosave_gate::AutosaveGate` reads
it on EVERY call, so a change takes effect without restarting any worker.

The persisted keys, bounds and defaults (`GENERAL_AUTOSAVE_*_KEY`, `AUTOSAVE_*_MIN/MAX/
DEFAULT`) and the pure reader `autosave_policy_from_user_settings` live in `lib.rs`, next
to the rest of the `General` section.

Key types:
- `AutosavePolicy`

Key functions:
- `autosave_policy()` / `set_autosave_interval_minutes()` / `set_autosave_action_threshold()`
- `seed_autosave_policy_from_user_settings()`
*/

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::{
    AUTOSAVE_ACTION_THRESHOLD_DEFAULT, AUTOSAVE_ACTION_THRESHOLD_MAX, AUTOSAVE_ACTION_THRESHOLD_MIN, AUTOSAVE_INTERVAL_MINUTES_DEFAULT, AUTOSAVE_INTERVAL_MINUTES_MAX,
    AUTOSAVE_INTERVAL_MINUTES_MIN, autosave_policy_from_user_settings,
};

/// One snapshot of the autosave policy.
///
/// Pending edits are flushed when `interval` has passed since the FIRST pending action
/// (not the last one), or when `action_threshold` actions have accumulated, whichever
/// comes first. An "action" is one enqueued save gesture (a stroke commit, a text edit,
/// a bubble change) — never a per-frame pixel mark. `action_threshold == 1` means every
/// action flushes immediately. Values produced by this module are always inside the
/// `AUTOSAVE_*_MIN..=MAX` bounds; a hand-built value (tests) may be anything, and
/// consumers must treat `action_threshold == 0` like `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutosavePolicy {
    /// Maximum age of the oldest pending action before a flush is due.
    pub interval: Duration,
    /// Number of pending actions that makes a flush due immediately.
    pub action_threshold: u32,
}

impl AutosavePolicy {
    /// The interval in whole minutes (rounded down), saturating at `u32::MAX`. Meant for
    /// the settings UI, which edits the interval in minutes.
    #[must_use]
    pub fn interval_minutes(&self) -> u32 {
        u32::try_from(self.interval.as_secs() / 60).unwrap_or(u32::MAX)
    }
}

/// Backing store of the interval, in minutes. Relaxed atomics suffice: the two knobs are
/// independent and a reader seeing one updated a moment before the other is harmless.
static INTERVAL_MINUTES: AtomicU32 = AtomicU32::new(AUTOSAVE_INTERVAL_MINUTES_DEFAULT);
/// Backing store of the action threshold.
static ACTION_THRESHOLD: AtomicU32 = AtomicU32::new(AUTOSAVE_ACTION_THRESHOLD_DEFAULT);

/// Returns the currently active autosave policy (always within the documented bounds).
#[must_use]
pub fn autosave_policy() -> AutosavePolicy {
    let minutes = INTERVAL_MINUTES.load(Ordering::Relaxed);
    AutosavePolicy { interval: Duration::from_secs(u64::from(minutes) * 60), action_threshold: ACTION_THRESHOLD.load(Ordering::Relaxed) }
}

/// Sets the autosave interval in minutes, clamped to
/// `AUTOSAVE_INTERVAL_MINUTES_MIN..=AUTOSAVE_INTERVAL_MINUTES_MAX`. Takes effect on the
/// next gate call; nothing is persisted here.
pub fn set_autosave_interval_minutes(minutes: u32) {
    INTERVAL_MINUTES.store(minutes.clamp(AUTOSAVE_INTERVAL_MINUTES_MIN, AUTOSAVE_INTERVAL_MINUTES_MAX), Ordering::Relaxed);
}

/// Sets the autosave action threshold, clamped to
/// `AUTOSAVE_ACTION_THRESHOLD_MIN..=AUTOSAVE_ACTION_THRESHOLD_MAX`. Takes effect on the
/// next gate call; nothing is persisted here.
pub fn set_autosave_action_threshold(threshold: u32) {
    ACTION_THRESHOLD.store(threshold.clamp(AUTOSAVE_ACTION_THRESHOLD_MIN, AUTOSAVE_ACTION_THRESHOLD_MAX), Ordering::Relaxed);
}

/// Seeds the runtime policy from a loaded `user_config.json` root. Missing or invalid
/// values resolve to the defaults / nearest bound (see
/// [`autosave_policy_from_user_settings`]).
pub fn seed_autosave_policy_from_user_settings(user_settings: &Value) {
    let policy = autosave_policy_from_user_settings(user_settings);
    set_autosave_interval_minutes(policy.interval_minutes());
    set_autosave_action_threshold(policy.action_threshold);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY, GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY};
    use serde_json::json;

    #[test]
    fn interval_minutes_roundtrips_and_rounds_down() {
        let policy = AutosavePolicy { interval: Duration::from_secs(5 * 60 + 59), action_threshold: 1 };
        assert_eq!(policy.interval_minutes(), 5);
    }

    #[test]
    fn reader_defaults_clamps_and_accepts_floats() {
        let defaults = autosave_policy_from_user_settings(&json!({}));
        assert_eq!(defaults.interval_minutes(), AUTOSAVE_INTERVAL_MINUTES_DEFAULT);
        assert_eq!(defaults.action_threshold, AUTOSAVE_ACTION_THRESHOLD_DEFAULT);

        let out_of_range = autosave_policy_from_user_settings(&json!({"General": {
            GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY: 0,
            GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY: 100_000,
        }}));
        assert_eq!(out_of_range.interval_minutes(), AUTOSAVE_INTERVAL_MINUTES_MIN);
        assert_eq!(out_of_range.action_threshold, AUTOSAVE_ACTION_THRESHOLD_MAX);

        let negative_and_float = autosave_policy_from_user_settings(&json!({"General": {
            GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY: 7.6,
            GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY: -4,
        }}));
        assert_eq!(negative_and_float.interval_minutes(), 8);
        assert_eq!(negative_and_float.action_threshold, AUTOSAVE_ACTION_THRESHOLD_MIN);

        let garbage = autosave_policy_from_user_settings(&json!({"General": {
            GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY: "soon",
            GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY: null,
        }}));
        assert_eq!(garbage.interval_minutes(), AUTOSAVE_INTERVAL_MINUTES_DEFAULT);
        assert_eq!(garbage.action_threshold, AUTOSAVE_ACTION_THRESHOLD_DEFAULT);
    }

    // The only test touching the process globals, so no other test in this crate can race it.
    #[test]
    fn setters_clamp_and_seed_applies_config() {
        set_autosave_interval_minutes(0);
        set_autosave_action_threshold(u32::MAX);
        assert_eq!(autosave_policy().interval, Duration::from_secs(u64::from(AUTOSAVE_INTERVAL_MINUTES_MIN) * 60));
        assert_eq!(autosave_policy().action_threshold, AUTOSAVE_ACTION_THRESHOLD_MAX);

        seed_autosave_policy_from_user_settings(&json!({"General": {
            GENERAL_AUTOSAVE_INTERVAL_MINUTES_KEY: 12,
            GENERAL_AUTOSAVE_ACTION_THRESHOLD_KEY: 7,
        }}));
        assert_eq!(autosave_policy(), AutosavePolicy { interval: Duration::from_secs(12 * 60), action_threshold: 7 });

        // Restore the defaults so later observers of the globals see the documented values.
        set_autosave_interval_minutes(AUTOSAVE_INTERVAL_MINUTES_DEFAULT);
        set_autosave_action_threshold(AUTOSAVE_ACTION_THRESHOLD_DEFAULT);
    }
}
