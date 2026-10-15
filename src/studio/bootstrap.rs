/*
File: src/studio/bootstrap.rs

Purpose:
`StudioBootstrap`: every disk read `StudioApp::new` needs, done BEFORE the constructor runs —
on the main window's open worker (`src/shell/studio/open.rs`, right after the project load) —
so the constructor, which runs inside a GUI frame, only wires state and spawns threads.

Key structures:
- `StudioBootstrap`: the merged startup `user_config` root, the input binding overrides, the
  Settings tab seed and its spellcheck word lists, the typing panels' seed and the two staging
  probes (the chapter's staging directory, the staging bubbles document).

Key functions:
- `StudioBootstrap::build_blocking(project)`: the reads, in the order the constructor used to
  make them. Blocking; never on the GUI thread of a running window (the web entry, which has no
  worker and an in-memory store, calls it inline).
- `load_hotkey_overrides_blocking`: the `Hotkeys` overrides out of the already-read root, and the
  ONE write this file makes: a version-1 section migrated to format 2 is written back once.

Notes:
Only reads whose RESULT can be taken a few milliseconds early live here; everything that must
observe state created during construction (the process-global spellcheck target, the memory
manager's profile, the shared models, every thread spawn) stays in `StudioApp::new`. The bubbles
staging probe is `ms-models`' own (`bubbles_model::staged_bubbles_exist`); this file only runs it
early and hands the answer to `BubblesModel::new_with_staging_probe`. Plain data: built on one
thread, consumed on another, and cheap to drop unused (a load discarded by a close).
*/

use crate::project::ProjectData;
use crate::runtime_log;
use crate::tabs::settings::{SettingsTabSeed, SpellcheckWordsSeed};
use crate::tabs::typing::TypingPanelSeed;
use ms_input::{HotkeyOverrides, LoadedOverrides, load_overrides, write_migrated_section};
use serde_json::Value;
use std::path::Path;

/// The pre-read inputs of [`super::StudioApp::new`] for one opened project.
#[derive(Debug)]
pub(crate) struct StudioBootstrap {
    /// The merged startup `user_config` root (`load_user_settings_for_startup`; an empty object,
    /// logged, when it cannot be read): memory profile, hint / centering flags, panel layouts,
    /// the image set's JPEG quality.
    pub(super) user_settings: Value,
    /// The user's input binding overrides (format 2; a stored version-1 section already
    /// migrated), applied to the input registry after every declaration is merged.
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
    /// the constructor's own reads did (defaults, logged). Writes only two migrations: a
    /// version-1 `Hotkeys` section is written back as format 2 (`load_hotkey_overrides_blocking`;
    /// skipped in this crate's own tests, which must not touch the real `user_config`), and with
    /// the `tutorial` feature the shared Settings panels load the tutorial progress through
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
        // The overrides come from the root read above (one `user_config` read, not two). The
        // migration write-back targets the real `user_config`, so this package's tests skip it:
        // the root package is the top of the crate graph, no other test binary links it with
        // `cfg(test)` off, so `cfg!(test)` is a complete guard here.
        let write_back = (!cfg!(test)).then(crate::config::user_config_path);
        let hotkey_overrides = load_hotkey_overrides_blocking(&user_settings, write_back.as_deref());
        let has_staged_bubbles = crate::models::bubbles_model::staged_bubbles_exist(&project.paths.unsaved_bubbles_file);
        Self { user_settings, hotkey_overrides, settings_seed, spellcheck_words, typing_seed, has_staging, has_staged_bubbles }
    }
}

/// The `Hotkeys` overrides stored in the loaded `user_config` root `user_settings`
/// (`ms_input::load_overrides`). When that section is still version 1 it is migrated in memory,
/// and the format-2 section is written back once to `write_back` (the `user_config` document;
/// `None` = do not write). Blocking: call it on a worker. A failed write is logged and changes
/// nothing else: the migrated overrides still apply, and the next start migrates again.
fn load_hotkey_overrides_blocking(user_settings: &Value, write_back: Option<&Path>) -> HotkeyOverrides {
    let LoadedOverrides { overrides, migrated_section } = load_overrides(user_settings);
    if let (Some(section), Some(path)) = (migrated_section, write_back) {
        match write_migrated_section(path, section) {
            Ok(()) => runtime_log::log_info(format!("[studio::bootstrap] Hotkeys section migrated to format 2 and written back; path={}", path.display())),
            Err(err) => runtime_log::log_warn(format!(
                "[studio::bootstrap] could not write the migrated Hotkeys section; the migrated bindings apply for this session and the migration is retried at the next start; path={} error={err}",
                path.display()
            )),
        }
    }
    overrides
}

/// Whether the staging directory `unsaved_dir` exists (`Path::exists`: an inspection error
/// reads as absent, the long-standing rule of the unsaved-changes flag).
fn staging_dir_exists(unsaved_dir: &Path) -> bool {
    unsaved_dir.exists()
}

#[cfg(test)]
mod tests {
    use super::{load_hotkey_overrides_blocking, staging_dir_exists};
    use serde_json::json;

    /// A unique scratch `user_config` holding `root` under the system temp dir: (its directory,
    /// removed by the caller, and the document path). Written through the same docstore the
    /// writers use, so the storage format is the one they pick.
    fn scratch_config(name: &str, root: &serde_json::Value) -> (std::path::PathBuf, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir().join(format!("ms_bootstrap_{name}_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join("user_config.json");
        let seed = root.clone();
        crate::config::update_user_config_file(&path, move |doc| {
            *doc = seed;
            Ok(())
        })
        .expect("seed scratch user_config");
        (dir, path)
    }

    /// The scratch document's current root.
    fn read_root(path: &std::path::Path) -> serde_json::Value {
        let doc = ms_docstore::DocRef::new(path, ms_docstore::DocKind::UserConfig);
        ms_docstore::read_value(&doc).expect("scratch user_config reads back").expect("scratch user_config exists")
    }

    /// A version-1 section is migrated in memory and written back once as format 2; the rest of
    /// the document is kept.
    #[test]
    fn a_version_1_hotkeys_section_is_migrated_and_written_back() {
        let root = json!({
            "General": { "ui_language": "ru" },
            "Hotkeys": { "translation.panel.bubbles.toggle": { "enabled": true, "shortcut": { "key": "B", "modifiers": {} } } }
        });
        let (dir, path) = scratch_config("v1", &root);
        let overrides = load_hotkey_overrides_blocking(&root, Some(&path));
        assert!(overrides.actions.contains_key("translation.panel.bubbles.toggle"), "{overrides:?}");
        let written = read_root(&path);
        assert_eq!(written["Hotkeys"]["format"], json!(ms_input::HOTKEYS_FORMAT));
        assert!(written["Hotkeys"]["actions"]["translation.panel.bubbles.toggle"].is_array(), "{written}");
        assert_eq!(written["General"]["ui_language"], json!("ru"));
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    /// A format-2 section is read as is and nothing is written; without a write-back target a
    /// version-1 section is migrated in memory only.
    #[test]
    fn a_format_2_section_or_no_target_writes_nothing() {
        let v2 = json!({ "Hotkeys": { "format": ms_input::HOTKEYS_FORMAT, "actions": { "translation.panel.ocr.toggle": [] } } });
        let (dir, path) = scratch_config("v2", &v2);
        let overrides = load_hotkey_overrides_blocking(&v2, Some(&path));
        assert_eq!(overrides.actions.get("translation.panel.ocr.toggle"), Some(&Vec::new()));
        assert_eq!(read_root(&path), v2, "a format-2 section is never rewritten");
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");

        let v1 = json!({ "Hotkeys": { "translation.panel.ocr.toggle": { "enabled": false } } });
        let (dir, path) = scratch_config("v1_no_target", &v1);
        let overrides = load_hotkey_overrides_blocking(&v1, None);
        assert_eq!(overrides.actions.get("translation.panel.ocr.toggle"), Some(&Vec::new()), "a disabled v1 entry migrates to unbound");
        assert_eq!(read_root(&path), v1, "no target: nothing is written");
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

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
