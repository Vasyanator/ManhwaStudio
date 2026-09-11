/*
File: crates/ms-project/src/project_scan.rs

Purpose:
Filesystem scan of the projects root shared by the launcher and by startup: enumerating titles
and chapters, validating that a chapter directory can be opened, and spotting an unsaved copy
of a chapter.

Key structures:
- `ProjectValidationState`: verdict of `validate_project_dir_for_startup`.

Key functions:
- `find_unsaved_chapter()`
- `validate_project_dir_for_startup()`
- `count_images_in_dir()`
- `list_titles()`
- `list_chapters()`

Notes:
These live outside `main.rs` so that `launcher` does not have to reference the binary root
upwards. Everything here is plain filesystem I/O and must stay free of UI and of app state.
*/

use ms_config as config;
use std::ffi::OsStr;
use std::path::Path;

/// Returns Some(chapter_name) when the given title folder contains a `{chapter}_unsaved` dir
/// that has a corresponding base chapter dir.
pub fn find_unsaved_chapter(projects_root: &Path, title: &str) -> Option<String> {
    let title_dir = projects_root.join(title);
    let entries = std::fs::read_dir(&title_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(base) = name_str.strip_suffix("_unsaved")
            && !base.is_empty()
            && title_dir.join(base).is_dir()
        {
            return Some(base.to_string());
        }
    }
    None
}

#[derive(Debug)]
pub enum ProjectValidationState {
    Valid { image_count: usize },
    Invalid { message: String },
}

pub fn validate_project_dir_for_startup(project_dir: &Path) -> ProjectValidationState {
    let src_dir = project_dir.join(config::SRC_DIR);
    if !src_dir.is_dir() {
        let scr_dir = project_dir.join("scr");
        if scr_dir.is_dir() {
            if let Err(err) = std::fs::rename(&scr_dir, &src_dir) {
                return ProjectValidationState::Invalid {
                    message: tf!("startup.validate.scr_rename_failed", project_dir = project_dir.display(), err = err),
                };
            }
        } else {
            return ProjectValidationState::Invalid {
                message: tf!("startup.validate.no_src_dir", project_dir = project_dir.display()),
            };
        }
    }

    match count_images_in_dir(&src_dir) {
        Ok(0) => ProjectValidationState::Invalid {
            message: tf!("startup.validate.no_images_in_src", project_dir = project_dir.display()),
        },
        Ok(image_count) => ProjectValidationState::Valid { image_count },
        Err(err) => ProjectValidationState::Invalid {
            message: tf!("startup.validate.check_failed", src_dir = src_dir.display(), err = err),
        },
    }
}

pub fn count_images_in_dir(dir: &Path) -> std::io::Result<usize> {
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            count += 1;
        }
    }
    Ok(count)
}

pub fn list_titles(projects_root: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(projects_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        out.push(name.to_string());
    }
    out.sort();
    Ok(out)
}

pub fn list_chapters(projects_root: &Path, title: &str) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let title_dir = projects_root.join(title);
    for entry in std::fs::read_dir(title_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name == "characters" {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    Ok(out)
}
