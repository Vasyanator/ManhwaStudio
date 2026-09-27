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

use ms_config as config;
use ms_log::runtime_log;

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

/// Writes the `Tutorials` section of the real user config (see [`persist_to_config_at`]).
/// Synchronous: runs on the background thread spawned by `persist`.
fn persist_to_config(completed: &[String], autoplay: bool) -> anyhow::Result<()> {
    persist_to_config_at(&config::user_config_path(), completed, autoplay)
}

/// ONE serialized read-modify-write of the user-config document at `path`: backfills the
/// missing default keys (the seeding `load_user_config` used to do before the write) and
/// replaces `Tutorials.completed` / `Tutorials.autoplay`. Every other key survives; a
/// malformed document is reported and never overwritten.
///
/// # Errors
/// A read/parse/write failure of the document, with its path in the context.
fn persist_to_config_at(path: &std::path::Path, completed: &[String], autoplay: bool) -> anyhow::Result<()> {
    config::update_user_config_file(path, |root| {
        config::merge_missing(root, &config::user_config_defaults());
        let root_obj = root.as_object_mut().ok_or_else(|| anyhow::anyhow!("user config root is not an object"))?;
        let section = root_obj.entry("Tutorials").or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !section.is_object() {
            *section = Value::Object(serde_json::Map::new());
        }
        if let Value::Object(section) = section {
            section.insert("completed".to_owned(), Value::Array(completed.iter().cloned().map(Value::String).collect()));
            section.insert("autoplay".to_owned(), Value::Bool(autoplay));
        }
        Ok(())
    })
}

/// Load progress once and wrap it in a shared handle for a surface to distribute
/// to its controller and settings pane.
#[must_use]
pub fn shared_progress() -> TutorialProgressHandle {
    Arc::new(Mutex::new(TutorialProgress::load()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persist_writes_the_section_once_keeps_other_keys_and_seeds_defaults() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("user_config.json");
        std::fs::write(&path, serde_json::to_vec(&json!({"General": {"theme": "light"}, "Tutorials": {"autoplay": true, "extra": 1}}))?)?;
        persist_to_config_at(&path, &["launcher_main".to_owned()], false)?;
        let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(value["Tutorials"]["completed"], json!(["launcher_main"]));
        assert_eq!(value["Tutorials"]["autoplay"], json!(false));
        assert_eq!(value["Tutorials"]["extra"], json!(1), "unrelated keys of the section survive");
        assert_eq!(value["General"]["theme"], json!("light"), "existing values are never replaced");
        assert!(value["General"].get("ui_scale_percent").is_some(), "defaults are backfilled");
        Ok(())
    }

    #[test]
    fn persist_never_overwrites_a_malformed_config() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("user_config.json");
        std::fs::write(&path, b"{ broken")?;
        assert!(persist_to_config_at(&path, &[], true).is_err());
        assert_eq!(std::fs::read(&path)?, b"{ broken");
        Ok(())
    }
}
