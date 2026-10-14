/*
File: src/shell/chapter_load.rs

Purpose:
The ONE owner of the "resume the unsaved session?" decision when a chapter directory is opened:
a `{chapter}_unsaved` staging folder next to the chapter means `ProjectData::load_resume_unsaved`,
its absence means `ProjectData::load`. No caller passes the answer in: the launcher's "open
fresh" choice is carried by the filesystem (it deletes the staging folder before it opens).

Key functions:
- `load_chapter(project_dir, user_settings)`: the probe + the matching load (blocking).
- `has_unsaved_staging(project_dir)`: the probe alone.

Notes:
Used by the studio's open worker (`studio/open.rs`) and by the web entry (`src/web_entry.rs`),
so it is compiled on every target. The probe asks the real filesystem (`Path::is_dir`); on wasm
that is always false, so the web entry always takes the plain `load` (it has no unsaved banner
either, for the same reason).
*/

use crate::project::ProjectData;
use std::ffi::OsStr;
use std::path::Path;

/// Returns true when there is a `{chapter}_unsaved` staging folder next to `project_dir`
/// (suffix owned by `ms_page_ops::chapter_docs::UNSAVED_DIR_SUFFIX`). A path without a file name
/// or a parent has no staging folder.
pub(crate) fn has_unsaved_staging(project_dir: &Path) -> bool {
    let Some(chapter_name) = project_dir.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(title_dir) = project_dir.parent() else {
        return false;
    };
    title_dir.join(format!("{chapter_name}{}", crate::page_ops::chapter_docs::UNSAVED_DIR_SUFFIX)).is_dir()
}

/// Loads the chapter at `project_dir`: `ProjectData::load_resume_unsaved` when
/// [`has_unsaved_staging`] holds (it refuses a damaged session with a localized error), else
/// `ProjectData::load`. Blocking (decode, journal recovery, renames): worker threads only on
/// native targets.
///
/// # Errors
/// The error of the chosen `ProjectData` load, without added context.
pub(crate) fn load_chapter(project_dir: &Path, user_settings: &serde_json::Value) -> anyhow::Result<ProjectData> {
    if has_unsaved_staging(project_dir) {
        ProjectData::load_resume_unsaved(project_dir, user_settings)
    } else {
        ProjectData::load(project_dir, user_settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh, uniquely named title directory under the system temp dir (removed by the test).
    fn temp_title(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-chapter-load-{tag}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear stale temp title");
        }
        std::fs::create_dir_all(&dir).expect("create temp title");
        dir
    }

    /// The probe sees only the chapter's own `{chapter}_unsaved` directory: not another
    /// chapter's, not a file of that name, not a path without a parent.
    #[test]
    fn probe_detects_only_the_chapters_own_staging_dir() {
        let title = temp_title("probe");
        let chapter = title.join("Chapter 1");
        std::fs::create_dir_all(&chapter).expect("create chapter");
        assert!(!has_unsaved_staging(&chapter), "no staging folder yet");

        std::fs::create_dir_all(title.join("Chapter 2_unsaved")).expect("create other staging");
        assert!(!has_unsaved_staging(&chapter), "another chapter's staging does not count");

        std::fs::write(title.join("Chapter 1_unsaved"), b"").expect("create staging-named file");
        assert!(!has_unsaved_staging(&chapter), "a file is not a staging folder");
        std::fs::remove_file(title.join("Chapter 1_unsaved")).expect("remove staging-named file");

        std::fs::create_dir_all(title.join("Chapter 1_unsaved")).expect("create own staging");
        assert!(has_unsaved_staging(&chapter), "the chapter's own staging folder is detected");

        assert!(!has_unsaved_staging(Path::new("/")), "a root path has no staging folder");
        std::fs::remove_dir_all(&title).expect("remove temp title");
    }
}
