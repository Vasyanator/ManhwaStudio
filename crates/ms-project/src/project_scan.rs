/*
File: crates/ms-project/src/project_scan.rs

Purpose:
Filesystem scan of the projects root shared by the launcher and by startup: enumerating titles
and chapters, validating that a chapter directory can be opened, spotting an unsaved copy
of a chapter, and probing / converting the storage format (JSON or SQLite) of a chapter's
owned documents.

Key structures:
- `ProjectValidationState`: verdict of `validate_project_dir_for_startup`.

Key functions:
- `find_unsaved_chapter()`
- `validate_project_dir_for_startup()`
- `chapter_storage_report()` / `convert_chapter_storage()`: format probe and conversion of
  the chapter's owned documents in BOTH trees (committed and `{chapter}_unsaved`)
- `damaged_unsaved_documents()` / `damaged_unsaved_session_message()`: parse probe of the
  `{chapter}_unsaved` staging documents (a damaged unsaved session is discardable, never
  silently substituted) and the user-facing error naming the damaged files
- `count_images_in_dir()`
- `list_titles()`
- `list_chapters()`

Notes:
The chapter's owned documents are named by `ms_page_ops::chapter_docs` (never re-derived
here). A chapter keeps its format across a global Dev/Prod switch; converting it is an
explicit user action (the launcher's open page), never a side effect of a scan.
These live outside `main.rs` so that `launcher` does not have to reference the binary root
upwards. Everything here is plain filesystem I/O and must stay free of UI and of app state.
*/

use ms_config as config;
use ms_docstore::{BatchOutcome, ChapterFormatReport, DocFormat, DocRef, DocStoreError};
use ms_log::runtime_log;
use ms_page_ops::chapter_docs;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// Number of committed documents at the head of `chapter_docs::chapter_doc_siblings`'s
/// result (committed bubbles, committed layers); the rest are the staging tree's.
const COMMITTED_SIBLING_COUNT: usize = 2;

/// The chapter's owned documents (both trees, rule-B.3 order) for the chapter directory
/// `project_dir`, or `None` when no chapter layout can be derived from the path.
fn chapter_owned_docs(project_dir: &Path) -> Option<[DocRef; 4]> {
    let (committed, unsaved) = chapter_docs::chapter_trees(project_dir)?;
    Some(chapter_docs::chapter_doc_siblings(&committed, &unsaved))
}

/// Which storage format each existing owned document of the chapter at `project_dir` uses,
/// over the committed tree AND its `{chapter}_unsaved` staging mirror. Absent documents are
/// omitted; a document whose format cannot be determined lands in `unreadable`.
///
/// Stat + header sniff only (no parse, no lock), but still filesystem I/O: call it on a
/// worker thread. A path without a chapter layout yields an empty report.
#[must_use]
pub fn chapter_storage_report(project_dir: &Path) -> ChapterFormatReport {
    let Some(docs) = chapter_owned_docs(project_dir) else { return ChapterFormatReport::default() };
    let (committed, staging) = docs.split_at(COMMITTED_SIBLING_COUNT);
    ms_docstore::chapter_format_report(committed, staging)
}

/// One owned document of a `{chapter}_unsaved` staging tree that exists but does not parse
/// (e.g. zero-length or truncated after a power loss: staging writes skip fsync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamagedStagingDoc {
    /// The damaged file (with its extension), as reported by the document store.
    pub path: PathBuf,
    /// Parser / header-sniff message.
    pub cause: String,
}

/// Fully reads (parses) every owned document of the `{chapter}_unsaved` staging tree of the
/// chapter at `chapter_dir` (either tree's directory is accepted) and returns the ones that
/// exist but are malformed. Absent documents and the committed tree are not checked, so a
/// non-empty result always means "the unsaved session is damaged" — never committed data.
///
/// Only `DocStoreError::Malformed` counts as damage: an I/O failure or a newer `.db` schema
/// is logged and not reported (discarding the session would not be the right remedy there).
/// Blocking I/O proportional to the documents' size: call it on a worker thread.
#[must_use]
pub fn damaged_unsaved_documents(chapter_dir: &Path) -> Vec<DamagedStagingDoc> {
    let Some(docs) = chapter_owned_docs(chapter_dir) else { return Vec::new() };
    let mut damaged = Vec::new();
    for doc in &docs[COMMITTED_SIBLING_COUNT..] {
        match ms_docstore::read_value(doc) {
            Ok(_) => {}
            Err(DocStoreError::Malformed { path, cause }) => {
                runtime_log::log_warn(format!("[project-scan] damaged unsaved-session document '{}': {cause}", path.display()));
                damaged.push(DamagedStagingDoc { path, cause });
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[project-scan] could not probe unsaved-session document '{}' (not treated as damage): {err}",
                    doc.stem().display()
                ));
            }
        }
    }
    damaged
}

/// Localized error for a studio load refused because the unsaved session of the chapter is
/// damaged: names every file in `damaged` and tells the user to discard the session through
/// the launcher (no file is deleted automatically).
#[must_use]
pub fn damaged_unsaved_session_message(damaged: &[DamagedStagingDoc]) -> String {
    let files: Vec<String> = damaged.iter().map(|doc| format!("{} ({})", doc.path.display(), doc.cause)).collect();
    tf!("startup.validate.unsaved_session_damaged", files = files.join("\n"))
}

/// Converts every owned document of the chapter at `project_dir` (committed and
/// `{chapter}_unsaved` trees) into `target`, continuing past per-document failures; each
/// failed document stays readable in its previous format. `progress(done, total)` is called
/// after each document on the calling thread.
///
/// Blocking I/O under the per-document locks: run it on a worker thread, and only while no
/// editor of this chapter is open in this process. A path without a chapter layout converts
/// nothing (empty outcome).
#[must_use]
pub fn convert_chapter_storage(project_dir: &Path, target: DocFormat, progress: &dyn Fn(usize, usize)) -> BatchOutcome {
    let Some(docs) = chapter_owned_docs(project_dir) else { return BatchOutcome::default() };
    ms_docstore::convert_many(&docs, target, progress)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use serde_json::json;

    /// Writes `value` as the `format` file of `doc` (creating parent dirs).
    fn put(doc: &DocRef, value: &serde_json::Value, format: DocFormat) {
        let path = doc.path_for(format);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        ms_docstore::write_whole_atomic(doc, value, format).expect("write doc");
    }

    fn chapter(root: &Path) -> std::path::PathBuf {
        root.join("title").join("ch1")
    }

    #[test]
    fn json_chapter_needs_conversion_to_db_and_db_chapter_does_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        put(&chapter_docs::layers_doc(&dir), &json!({"pages": []}), DocFormat::Json);
        put(&chapter_docs::bubbles_doc(&dir), &json!({"bubbles": []}), DocFormat::Json);
        let report = chapter_storage_report(&dir);
        assert_eq!(report.committed.len(), 2);
        assert!(report.staging.is_empty());
        assert!(report.needs_conversion(DocFormat::Db));
        assert!(!report.needs_conversion(DocFormat::Json));

        let other = tmp.path().join("title").join("ch2");
        put(&chapter_docs::layers_doc(&other), &json!({"pages": []}), DocFormat::Db);
        let report = chapter_storage_report(&other);
        assert!(!report.needs_conversion(DocFormat::Db));
        assert!(report.needs_conversion(DocFormat::Json));
    }

    #[test]
    fn staging_mismatch_is_detected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        put(&chapter_docs::layers_doc(&dir), &json!({"pages": []}), DocFormat::Db);
        let staging = tmp.path().join("title").join("ch1_unsaved");
        put(&chapter_docs::bubbles_doc(&staging), &json!({"bubbles": []}), DocFormat::Json);
        let report = chapter_storage_report(&dir);
        assert_eq!(report.committed, vec![(ms_docstore::DocKind::Layers, DocFormat::Db)]);
        assert_eq!(report.staging, vec![(ms_docstore::DocKind::Bubbles, DocFormat::Json)]);
        assert!(report.needs_conversion(DocFormat::Db));
    }

    #[test]
    fn malformed_db_is_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        let doc = chapter_docs::bubbles_doc(&dir);
        let path = doc.path_for(DocFormat::Db);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"not a database").expect("write");
        let report = chapter_storage_report(&dir);
        assert_eq!(report.unreadable.len(), 1);
        assert!(report.committed.is_empty());
    }

    #[test]
    fn empty_chapter_needs_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = chapter_storage_report(&chapter(tmp.path()));
        assert_eq!(report, ChapterFormatReport::default());
        assert!(!report.needs_conversion(DocFormat::Db));
    }

    #[test]
    fn conversion_covers_both_trees_and_preserves_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        let staging = tmp.path().join("title").join("ch1_unsaved");
        let layers = json!({"pages": [{"name": "001.png"}]});
        put(&chapter_docs::layers_doc(&dir), &layers, DocFormat::Json);
        put(&chapter_docs::bubbles_doc(&staging), &json!({"bubbles": [{"id": "a", "text": "hi"}]}), DocFormat::Json);
        let calls = std::cell::Cell::new(0usize);
        let outcome = convert_chapter_storage(&dir, DocFormat::Db, &|done, total| {
            assert_eq!(total, 4);
            calls.set(done);
        });
        assert_eq!(calls.get(), 4);
        assert_eq!(outcome.converted, 2);
        assert_eq!(outcome.skipped, 2);
        assert!(outcome.failed.is_empty());
        let report = chapter_storage_report(&dir);
        assert!(!report.needs_conversion(DocFormat::Db));
        assert_eq!(report.committed.len() + report.staging.len(), 2);
        let doc = chapter_docs::layers_doc(&dir);
        assert!(!doc.path_for(DocFormat::Json).exists());
        let back = ms_docstore::read_value(&doc).expect("read").expect("present");
        assert_eq!(back, layers);
    }

    #[test]
    fn conversion_reports_a_malformed_document_and_converts_the_rest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        put(&chapter_docs::layers_doc(&dir), &json!({"pages": []}), DocFormat::Json);
        let bubbles = chapter_docs::bubbles_doc(&dir);
        std::fs::write(bubbles.path_for(DocFormat::Json), b"{ broken").expect("write");
        let outcome = convert_chapter_storage(&dir, DocFormat::Db, &|_, _| {});
        assert_eq!(outcome.converted, 1);
        assert_eq!(outcome.failed.len(), 1);
        assert!(bubbles.path_for(DocFormat::Json).is_file());
    }

    /// Writes raw `bytes` as the JSON file of `doc` (creating parent dirs).
    fn put_raw(doc: &DocRef, bytes: &[u8]) -> PathBuf {
        let path = doc.path_for(DocFormat::Json);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn zero_length_staging_layers_and_malformed_staging_bubbles_are_damage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        let staging = tmp.path().join("title").join("ch1_unsaved");
        put(&chapter_docs::layers_doc(&dir), &json!({"pages": []}), DocFormat::Json);
        let layers = put_raw(&chapter_docs::layers_doc(&staging), b"");
        let bubbles = put_raw(&chapter_docs::bubbles_doc(&staging), b"{ \"pages\": [");
        // Either tree's directory addresses the same staging documents.
        for probe_dir in [&dir, &staging] {
            let damaged = damaged_unsaved_documents(probe_dir);
            let mut paths: Vec<&Path> = damaged.iter().map(|doc| doc.path.as_path()).collect();
            paths.sort();
            let mut expected = vec![bubbles.as_path(), layers.as_path()];
            expected.sort();
            assert_eq!(paths, expected);
        }
    }

    #[test]
    fn committed_damage_and_intact_staging_are_not_session_damage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        let staging = tmp.path().join("title").join("ch1_unsaved");
        put_raw(&chapter_docs::layers_doc(&dir), b"");
        put_raw(&chapter_docs::bubbles_doc(&dir), b"{ broken");
        assert!(damaged_unsaved_documents(&dir).is_empty());
        put(&chapter_docs::bubbles_doc(&staging), &json!({"pages": []}), DocFormat::Json);
        put(&chapter_docs::layers_doc(&staging), &json!({"pages": []}), DocFormat::Db);
        assert!(damaged_unsaved_documents(&dir).is_empty());
    }

    #[test]
    fn malformed_staging_db_is_damage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = chapter(tmp.path());
        let staging = tmp.path().join("title").join("ch1_unsaved");
        let doc = chapter_docs::bubbles_doc(&staging);
        let path = doc.path_for(DocFormat::Db);
        std::fs::create_dir_all(&staging).expect("mkdir");
        std::fs::write(&path, b"not a sqlite file").expect("write");
        let damaged = damaged_unsaved_documents(&dir);
        assert_eq!(damaged.len(), 1);
        assert_eq!(damaged[0].path, path);
    }
}
