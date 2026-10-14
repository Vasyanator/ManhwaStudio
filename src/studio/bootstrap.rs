/*
File: src/studio/bootstrap.rs

Purpose:
`StudioBootstrap`: every disk read `StudioApp::new` needs, done BEFORE the constructor runs —
on the main window's open worker (`src/shell/studio/open.rs`, right after the project load) —
so the constructor, which runs inside a GUI frame, only wires state and spawns threads.

Key structures:
- `StudioBootstrap`: the merged startup `user_config` root, the hotkey overrides, the Settings
  tab seed and its spellcheck word lists, the typing panels' seed and the two staging probes
  (the chapter's staging directory, the staging bubbles document).

Key functions:
- `StudioBootstrap::build_blocking(project)`: the reads, in the order the constructor used to
  make them. Blocking; never on the GUI thread of a running window (the web entry, which has no
  worker and an in-memory store, calls it inline).

Notes:
Only reads whose RESULT can be taken a few milliseconds early live here; everything that must
observe state created during construction (the process-global spellcheck target, the memory
manager's profile, the shared models, every thread spawn) stays in `StudioApp::new`. The bubbles
staging probe is `ms-models`' own (`bubbles_model::staged_bubbles_exist`); this file only runs it
early and hands the answer to `BubblesModel::new_with_staging_probe`. Plain data: built on one
thread, consumed on another, and cheap to drop unused (a load discarded by a close).
*/

use crate::input_manager_v2::HotkeyOverrides;
use crate::project::ProjectData;
use crate::runtime_log;
use crate::tabs::settings::{SettingsTabSeed, SpellcheckWordsSeed};
use crate::tabs::typing::TypingPanelSeed;
use serde_json::Value;
use std::path::Path;

/// The pre-read inputs of [`super::StudioApp::new`] for one opened project.
#[derive(Debug)]
pub(crate) struct StudioBootstrap {
    /// The merged startup `user_config` root (`load_user_settings_for_startup`; an empty object,
    /// logged, when it cannot be read): memory profile, hint / centering flags, panel layouts,
    /// the image set's JPEG quality.
    pub(super) user_settings: Value,
    /// The user's hotkey overrides, applied after every spec is registered.
    pub(super) hotkey_overrides: HotkeyOverrides,
    /// The Settings tab's construction-time reads.
    pub(super) settings_seed: SettingsTabSeed,
    /// The spellcheck word lists of the user and of this project's settings document.
    pub(super) spellcheck_words: SpellcheckWordsSeed,
    /// The typing panels' `user_config.TextTab` seed.
    pub(super) typing_seed: TypingPanelSeed,
    /// Whether the chapter's `{chapter}_unsaved` staging directory existed when the project
    /// opened: the project session's initial "unsaved changes" flag (ignored by an image set).
    pub(super) has_staging: bool,
    /// Whether the staging bubbles DOCUMENT (`ProjectPaths::unsaved_bubbles_file`, any docstore
    /// format) existed when the project opened: `BubblesModel`'s initial "unsaved changes" flag.
    /// A different fact from `has_staging` — the directory can exist without that document.
    pub(super) has_staged_bubbles: bool,
}

impl StudioBootstrap {
    /// Performs every read `StudioApp::new` needs for `project`. Blocking (document reads, a
    /// locale folder scan, one directory stat, one document-existence probe); run it on a worker. Failures degrade exactly as
    /// the constructor's own reads did (defaults, logged); nothing here writes, except that
    /// with the `tutorial` feature the shared Settings panels load the tutorial progress through
    /// `ms_config::load_user_config`, which may persist a config migration (as it always did).
    #[must_use]
    pub(crate) fn build_blocking(project: &ProjectData) -> Self {
        let user_settings = crate::config::load_user_settings_for_startup().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[memory] failed to load user settings for memory profile; using default: {err}"
            ));
            Value::Object(serde_json::Map::new())
        });
        let typing_seed = TypingPanelSeed::read_blocking();
        let settings_seed = SettingsTabSeed::read_blocking();
        let spellcheck_words = SpellcheckWordsSeed::read_blocking(&project.paths.settings_file);
        let has_staging = staging_dir_exists(&project.paths.unsaved_dir);
        let hotkey_overrides = HotkeyOverrides::read_blocking(&crate::config::user_config_path());
        let has_staged_bubbles = crate::models::bubbles_model::staged_bubbles_exist(&project.paths.unsaved_bubbles_file);
        Self { user_settings, hotkey_overrides, settings_seed, spellcheck_words, typing_seed, has_staging, has_staged_bubbles }
    }
}

/// Whether the staging directory `unsaved_dir` exists (`Path::exists`: an inspection error
/// reads as absent, the long-standing rule of the unsaved-changes flag).
fn staging_dir_exists(unsaved_dir: &Path) -> bool {
    unsaved_dir.exists()
}

#[cfg(test)]
mod tests {
    use super::staging_dir_exists;

    /// The staging probe answers for the chapter's `_unsaved` directory itself.
    #[test]
    fn the_staging_probe_follows_the_unsaved_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let unsaved = dir.path().join("Chapter 1_unsaved");
        assert!(!staging_dir_exists(&unsaved));
        std::fs::create_dir_all(&unsaved).expect("create staging dir");
        assert!(staging_dir_exists(&unsaved));
    }

    /// The full read over a real temp chapter: the staging flag follows the chapter, and the
    /// rest only reads (the runtime root's `user_config` and spellcheck dictionary, whatever
    /// this machine has — absent reads as defaults). Off with `tutorial`: the shared Settings
    /// panels then load the tutorial progress through `load_user_config`, which may write a
    /// migration into the real `user_config` (test hygiene, global §16).
    #[cfg(not(feature = "tutorial"))]
    #[test]
    fn build_blocking_reads_the_staging_state_of_the_opened_chapter() {
        use super::StudioBootstrap;
        let dir = tempfile::tempdir().expect("temp dir");
        let chapter = dir.path().join("Title").join("Chapter 1");
        std::fs::create_dir_all(chapter.join("src")).expect("chapter dir");
        image::RgbImage::from_pixel(4, 3, image::Rgb([200, 100, 50])).save(chapter.join("src").join("001.png")).expect("write test png");
        let project = crate::project::ProjectData::load(&chapter, &serde_json::Value::Null).expect("the fixture chapter loads");
        let fresh = StudioBootstrap::build_blocking(&project);
        assert!(!fresh.has_staging, "a fresh chapter has no staging directory");
        assert!(!fresh.has_staged_bubbles, "a fresh chapter has no staging bubbles document");
        assert!(fresh.user_settings.is_object(), "the startup root is always an object");
        std::fs::create_dir_all(&project.paths.unsaved_dir).expect("create staging dir");
        let staged_dir = StudioBootstrap::build_blocking(&project);
        assert!(staged_dir.has_staging, "an existing staging directory is reported");
        assert!(!staged_dir.has_staged_bubbles, "an empty staging directory holds no bubbles document");
        crate::models::bubbles_model::write_bubbles_snapshot_to(&project.paths.unsaved_bubbles_file, &[]).expect("write staging bubbles");
        assert!(StudioBootstrap::build_blocking(&project).has_staged_bubbles, "an existing staging bubbles document is reported");
    }
}
