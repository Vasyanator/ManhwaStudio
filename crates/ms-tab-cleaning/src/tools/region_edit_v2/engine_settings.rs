/*
File: region_edit_v2/engine_settings.rs

Purpose:
Engine-agnostic helpers for the settings file every hosted `AiEngine` persists. Each engine owns
its own settings document, path and IO; this file owns only the rules that are the same for all
of them, so a new engine reuses them instead of carrying another copy.

Key functions:
- `settings_save_due()` — the save gate an engine's `poll` consults before starting a write

Notes:
GUI-free and IO-free: pure decisions only. The save itself stays in the engine, on its own
worker thread.
*/

/// Whether a settings save must be started right now.
///
/// `dirty` is raised by every parameter change (and by any engine-side rewrite of its own
/// settings, e.g. an OOM recovery). `settings_loaded` gates it because the initial load runs on
/// its own worker: saving the in-memory DEFAULTS before that load lands would overwrite the
/// user's file with defaults — a silent data loss rather than a visible failure.
/// `save_in_flight` keeps at most one writer on the file at a time. There is no time debounce:
/// a save starts on the first poll it is due on.
#[must_use]
pub(in crate::tools) fn settings_save_due(dirty: bool, settings_loaded: bool, save_in_flight: bool) -> bool {
    dirty && settings_loaded && !save_in_flight
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The save gate: only a real change writes, never before the initial load has landed,
    /// and never a second writer while one is in flight.
    #[test]
    fn a_save_is_due_only_after_the_load_and_never_twice_at_once() {
        assert!(settings_save_due(true, true, false));
        assert!(!settings_save_due(false, true, false), "nothing changed: no write");
        assert!(
            !settings_save_due(true, false, false),
            "saving before the load lands would overwrite the file with defaults"
        );
        assert!(!settings_save_due(true, true, true), "at most one writer at a time");
    }
}
