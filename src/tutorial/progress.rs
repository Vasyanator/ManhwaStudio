/*
File: src/tutorial/progress.rs

Purpose:
Persisted tutorial progress: the set of completed tutorials plus the autoplay
flag. Backed by the `Tutorials` section of `user_config.json`.

Key items:
- `TutorialProgress` — completed-set + autoplay, with mutators that persist.
- `TutorialProgressHandle` — shared handle so a surface's controller and its
  settings pane observe the same live state within one process run.
- `shared_progress()` — load once and wrap in a shared handle.

Notes:
Mutations are rare (finishing/skipping/resetting a tutorial, toggling autoplay),
so each mutator persists the full state. The file write is offloaded to a
background thread to keep the GUI thread free of I/O (project rule: no file I/O
on the GUI thread). Cross-process handoff (launcher run -> studio run) goes
through the config file; the in-memory handle bridges the two consumers within a
single surface.
*/

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::config;
use crate::runtime_log;

use super::id::TutorialId;

/// Shared, mutable progress observed by both a surface's `TutorialController` and
/// its settings pane, so a reset in settings is seen by autoplay immediately.
pub type TutorialProgressHandle = Arc<Mutex<TutorialProgress>>;

/// Persisted onboarding progress: which tutorials are done and whether unseen
/// tutorials auto-start on first entry.
#[derive(Debug, Clone)]
pub struct TutorialProgress {
    completed: HashSet<TutorialId>,
    autoplay: bool,
}

impl Default for TutorialProgress {
    fn default() -> Self {
        Self {
            completed: HashSet::new(),
            autoplay: true,
        }
    }
}

impl TutorialProgress {
    /// Load progress from the user config, best-effort. Unknown keys are ignored;
    /// a missing section or a read error yields defaults (nothing completed,
    /// autoplay on) so onboarding still works for a fresh or unreadable config.
    #[must_use]
    pub fn load() -> Self {
        let cfg = match config::load_user_config() {
            Ok(cfg) => cfg,
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[tutorial] failed to load progress from config, using defaults: {err:#}"
                ));
                return Self::default();
            }
        };
        let completed = cfg
            .get_path(&["Tutorials", "completed"])
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(TutorialId::from_key)
                    .collect()
            })
            .unwrap_or_default();
        let autoplay = cfg
            .get_path(&["Tutorials", "autoplay"])
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Self {
            completed,
            autoplay,
        }
    }

    /// Whether `id` has been finished or skipped.
    #[must_use]
    pub fn is_completed(&self, id: TutorialId) -> bool {
        self.completed.contains(&id)
    }

    /// Whether unseen tutorials auto-start on first entry to their surface/tab.
    #[must_use]
    pub fn autoplay(&self) -> bool {
        self.autoplay
    }

    /// Mark `id` completed and persist. No-op (and no write) if already recorded.
    pub fn mark_completed(&mut self, id: TutorialId) {
        if self.completed.insert(id) {
            self.persist();
        }
    }

    /// Clear `id`'s completion and persist, re-enabling autoplay on next entry.
    /// No-op (and no write) if it was not recorded.
    pub fn reset(&mut self, id: TutorialId) {
        if self.completed.remove(&id) {
            self.persist();
        }
    }

    /// Set the autoplay flag and persist. No-op if unchanged.
    pub fn set_autoplay(&mut self, autoplay: bool) {
        if self.autoplay != autoplay {
            self.autoplay = autoplay;
            self.persist();
        }
    }

    /// Write the full current state to the config file on a background thread so
    /// the GUI thread never blocks on file I/O. A stale-overwrite race between
    /// two near-simultaneous mutations is acceptable here: each write is a
    /// complete valid snapshot and these events are user-driven and seconds
    /// apart.
    fn persist(&self) {
        let completed: Vec<String> = self
            .completed
            .iter()
            .map(|id| id.key().to_string())
            .collect();
        let autoplay = self.autoplay;
        std::thread::spawn(move || {
            if let Err(err) = persist_to_config(&completed, autoplay) {
                runtime_log::log_warn(format!("[tutorial] failed to persist progress: {err:#}"));
            }
        });
    }
}

/// Load-modify-write the `Tutorials` section. `set_path` saves synchronously, so
/// this runs on the background thread spawned by `persist`.
fn persist_to_config(completed: &[String], autoplay: bool) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    cfg.set_path(
        &["Tutorials", "completed"],
        Value::Array(completed.iter().cloned().map(Value::String).collect()),
    )?;
    cfg.set_path(&["Tutorials", "autoplay"], Value::Bool(autoplay))?;
    Ok(())
}

/// Load progress once and wrap it in a shared handle for a surface to distribute
/// to its controller and settings pane.
#[must_use]
pub fn shared_progress() -> TutorialProgressHandle {
    Arc::new(Mutex::new(TutorialProgress::load()))
}
