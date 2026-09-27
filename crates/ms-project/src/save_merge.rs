/*
File: save_merge.rs

Purpose:
The "save to project" merge of a chapter: folds the `{chapter}_unsaved/` staging mirror into
the committed chapter directory, then removes the staging directory.

Main responsibilities:
- byte-copy every staged file that is NOT an owned document file (`.json`, `.db`, the `.db`'s
  `-journal`) nor a crash-leftover docstore temp file (`ms_docstore::is_temp_artifact`) over
  the committed tree;
- copy the staged bubbles document through `ms_docstore::copy_document` (validated, atomic,
  written in the committed document's format — or, when the committed chapter has none, the
  chapter's format per `ms_page_ops::chapter_docs` — directory fsynced because its staging
  copy is deleted right after);
- hand the layer manifest to the caller's per-page merge (it lives in `ms-models`, ABOVE this
  crate, so it is injected as a closure instead of being called directly);
- remove the staging directory once everything above succeeded.

Key functions:
- merge_unsaved_into_project()
- copy_dir_overwrite_except(), is_owned_document_file() (private)

Notes:
Native-only in practice: uses `std::fs` directly (recursive copy + `remove_dir_all`), exactly
like the code it was moved from (`src/app.rs`). Blocking; callers run it on a worker thread.
User-facing error texts reuse the `app.merge.*` localization keys.
Durability: only the bubbles document is directory-fsynced before the staging directory is
removed; the byte-copied page files and the per-page layer merge are not (a known gap, see
`dev-docs/known_gaps.md`).
*/

use ms_docstore::{DocKind, DocRef, Durability};
use ms_page_ops::chapter_docs;
use std::fs;
use std::path::{Path, PathBuf};

/// Merges the chapter's staging directory `unsaved_dir` into `project_dir` (the committed
/// chapter directory) and removes `unsaved_dir`. A missing `unsaved_dir` is a successful no-op.
///
/// Steps, in order (the first failure aborts and leaves `unsaved_dir` in place):
/// 1. every staged file is byte-copied over the committed tree (overwriting), EXCEPT the owned
///    documents — `layers/layers` and the bubbles document, as `.json`, `.db` or a `.db`'s
///    `-journal` — and docstore temp files left by a crash mid-write;
/// 2. the staged bubbles document (either format) is copied with `ms_docstore::copy_document`
///    with `Durability::ContentsAndDirectory` (its staging copy is deleted in step 4, so the new
///    directory entry must be durable first) INTO THE COMMITTED DOCUMENT'S FORMAT — its existing
///    one, else the chapter's (rule B.3) — so the committed chapter never holds both
///    `X.json` and `X.db`; a malformed staging document is an error and nothing is written over
///    the committed one;
/// 3. `merge_layers(committed_layers_dir, unsaved_layers_dir)` merges the layer manifest PER PAGE
///    (never a file overwrite: the staging manifest holds only the pages the session visited, a
///    blind copy would drop committed-only pages). The binary passes
///    `ms_models::layer_model::persist::merge_unsaved_layers_into_committed`; its `Err` text is
///    wrapped into the localized `app.merge.layers_merge_error`;
/// 4. `unsaved_dir` is removed.
///
/// # Errors
/// A localized, user-facing message naming the failing path and OS/store error.
pub fn merge_unsaved_into_project(
    unsaved_dir: &Path,
    project_dir: &Path,
    merge_layers: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<(), String> {
    if !unsaved_dir.is_dir() {
        // Nothing to merge — treat as success.
        return Ok(());
    }
    let committed_layers = project_dir.join(ms_config::LAYERS_DIR);
    let unsaved_layers = unsaved_dir.join(ms_config::LAYERS_DIR);
    let staged_bubbles = chapter_docs::bubbles_doc(unsaved_dir);
    // An existing committed document keeps its format (`copy_document` writes into it); a
    // committed chapter WITHOUT a bubbles document gets one in the chapter's format (rule B.3:
    // committed layers first, then the staging documents), never a second format next to a sibling.
    let committed_bubbles = chapter_docs::bubbles_doc(project_dir).with_new_format(ms_docstore::chapter_new_format(&chapter_docs::chapter_doc_siblings(project_dir, unsaved_dir)));
    // Owned documents are skipped by STEM, so both formats (`.json` / `.db`) and a `.db`'s own
    // `-journal` are excluded from the byte copy.
    let owned_stems = [chapter_docs::layers_doc(unsaved_dir).stem().to_path_buf(), staged_bubbles.stem().to_path_buf()];
    copy_dir_overwrite_except(unsaved_dir, project_dir, &owned_stems)?;
    ms_docstore::copy_document(&staged_bubbles, &committed_bubbles, Durability::ContentsAndDirectory).map_err(|e| {
        tf!(
            "app.merge.copy_error",
            src_path = staged_bubbles.path_for(ms_docstore::DocFormat::Json).display(),
            dst_path = committed_bubbles.path_for(ms_docstore::DocFormat::Json).display(),
            e = e
        )
    })?;
    merge_layers(&committed_layers, &unsaved_layers).map_err(|e| tf!("app.merge.layers_merge_error", e = e))?;
    fs::remove_dir_all(unsaved_dir).map_err(|e| tf!("app.merge.remove_temp_error", unsaved_dir = unsaved_dir.display(), e = e))?;
    Ok(())
}

/// Whether `path` is a file of one of the owned documents `skip_stems` (extension-less paths):
/// its `.json`, its `.db`, or the `.db`'s `SQLite` rollback journal `<stem>.db-journal`. The
/// journal must never be copied: SQLite would replay a foreign hot journal into the committed
/// `.db` of that name and corrupt it.
fn is_owned_document_file(path: &Path, skip_stems: &[PathBuf]) -> bool {
    let name = path.as_os_str().to_string_lossy();
    let path = name.strip_suffix("-journal").map_or_else(|| path.to_path_buf(), PathBuf::from);
    // The kind is irrelevant here: only the extension-stripped stem is compared.
    let doc = DocRef::new(&path, DocKind::Layers);
    skip_stems.iter().any(|skip| skip.as_path() == doc.stem())
}

/// Recursively copies files from `src` into `dst`, creating subdirectories as needed. Existing
/// files in `dst` are overwritten. A file whose document stem (path without a `.json`/`.db`
/// extension) is in `skip_stems` is NOT copied — owned documents are merged separately — and
/// neither is a docstore temp file (`.{name}.{pid}.tmp`, a crash leftover nothing would ever
/// remove from the committed tree).
fn copy_dir_overwrite_except(src: &Path, dst: &Path, skip_stems: &[PathBuf]) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| tf!("app.merge.create_dir_error", dst = dst.display(), e = e))?;
    let entries = fs::read_dir(src).map_err(|e| tf!("app.merge.read_dir_error", src = src.display(), e = e))?;
    for entry in entries {
        let entry = entry.map_err(|e| tf!("app.merge.read_entry_error", src = src.display(), e = e))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_overwrite_except(&src_path, &dst_path, skip_stems)?;
            continue;
        }
        if is_owned_document_file(&src_path, skip_stems) || ms_docstore::is_temp_artifact(&src_path) {
            continue;
        }
        fs::copy(&src_path, &dst_path)
            .map_err(|e| tf!("app.merge.copy_error", src_path = src_path.display(), dst_path = dst_path.display(), e = e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::merge_unsaved_into_project;
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;

    /// Lists every file under `dir` (relative, sorted), for leftover checks.
    fn files_under(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).expect("read_dir of a test fixture") {
                let path = entry.expect("dir entry of a test fixture").path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(path.strip_prefix(base).expect("path under base").to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn merge_copies_files_and_bubbles_skips_manifest_and_removes_staging() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        fs::create_dir_all(project.join("layers")).expect("mkdir");
        fs::create_dir_all(unsaved.join("layers")).expect("mkdir");
        fs::create_dir_all(unsaved.join("text_images")).expect("mkdir");
        fs::write(project.join("layers/layers.json"), b"{\"committed\":true}").expect("write");
        fs::write(project.join("translation_bubbles.json"), b"[]").expect("write");
        fs::write(unsaved.join("layers/layers.json"), b"{\"staged\":true}").expect("write");
        fs::write(unsaved.join("layers/000_a.png"), b"png").expect("write");
        fs::write(unsaved.join("text_images/000.png"), b"txt").expect("write");
        // Crash leftovers of the docstore write recipe must not reach the committed tree.
        fs::write(unsaved.join("layers/.layers.json.4242.tmp"), b"{").expect("write");
        fs::write(unsaved.join(".translation_bubbles.json.4242.tmp"), b"[").expect("write");
        let staged_bubbles = b"[\n  {\n    \"id\": 7\n  }\n]";
        fs::write(unsaved.join("translation_bubbles.json"), staged_bubbles).expect("write");

        let called = Cell::new(false);
        merge_unsaved_into_project(&unsaved, &project, |committed, staging| {
            assert_eq!(committed, project.join("layers"));
            assert_eq!(staging, unsaved.join("layers"));
            // The manifest must reach the per-page merge untouched by the byte copy.
            assert_eq!(fs::read(committed.join("layers.json")).expect("read"), b"{\"committed\":true}");
            called.set(true);
            Ok(())
        })
        .expect("merge");

        assert!(called.get());
        assert!(!unsaved.exists(), "staging dir removed");
        assert_eq!(fs::read(project.join("translation_bubbles.json")).expect("read"), staged_bubbles);
        assert_eq!(fs::read(project.join("layers/000_a.png")).expect("read"), b"png");
        assert_eq!(fs::read(project.join("text_images/000.png")).expect("read"), b"txt");
        assert_eq!(
            files_under(&project),
            vec!["layers/000_a.png", "layers/layers.json", "text_images/000.png", "translation_bubbles.json"],
            "no leftover temp files"
        );
    }

    #[test]
    fn missing_staging_dir_is_a_no_op() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        fs::create_dir_all(&project).expect("mkdir");
        merge_unsaved_into_project(&tmp.path().join("ch1_unsaved"), &project, |_, _| panic!("layers merge must not run"))
            .expect("no-op");
        assert!(files_under(&project).is_empty());
    }

    #[test]
    fn malformed_staged_bubbles_abort_and_keep_everything() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        fs::create_dir_all(&project).expect("mkdir");
        fs::create_dir_all(&unsaved).expect("mkdir");
        fs::write(project.join("translation_bubbles.json"), b"[{\"id\":1}]").expect("write");
        fs::write(unsaved.join("translation_bubbles.json"), b"[{\"id\":").expect("write");

        // The message is localized (no catalog is loaded in unit tests, so only the outcome is checked).
        merge_unsaved_into_project(&unsaved, &project, |_, _| panic!("layers merge must not run"))
            .expect_err("malformed staging bubbles must fail the merge");
        assert_eq!(fs::read(project.join("translation_bubbles.json")).expect("read"), b"[{\"id\":1}]");
        assert!(unsaved.join("translation_bubbles.json").exists(), "staging kept for a retry");
    }

    /// Writes `value` as the `format` file of `doc` (a fresh document).
    fn put(doc: &ms_docstore::DocRef, value: &serde_json::Value, format: ms_docstore::DocFormat) {
        ms_docstore::write_whole_atomic(doc, value, format).expect("write fixture document");
    }

    #[test]
    fn bubbles_merge_lands_in_the_committed_format_for_every_pair() {
        use ms_docstore::DocFormat;
        use ms_page_ops::chapter_docs::bubbles_doc;
        for committed_format in [DocFormat::Json, DocFormat::Db] {
            for staging_format in [DocFormat::Json, DocFormat::Db] {
                let case = format!("committed {committed_format:?}, staging {staging_format:?}");
                let tmp = tempfile::tempdir().expect("tempdir");
                let project = tmp.path().join("ch1");
                let unsaved = tmp.path().join("ch1_unsaved");
                put(&bubbles_doc(&project), &serde_json::json!([{"id": 1}]), committed_format);
                put(&bubbles_doc(&unsaved), &serde_json::json!([{"id": 7, "text": "x"}]), staging_format);
                merge_unsaved_into_project(&unsaved, &project, |_, _| Ok(())).expect("merge");
                let other = if committed_format == DocFormat::Json { DocFormat::Db } else { DocFormat::Json };
                let doc = bubbles_doc(&project);
                assert!(doc.path_for(committed_format).is_file(), "{case}: committed format kept");
                assert!(!doc.path_for(other).exists(), "{case}: no second format left in the committed chapter");
                assert_eq!(ms_docstore::read_value(&doc).expect("read"), Some(serde_json::json!([{"id": 7, "text": "x"}])), "{case}");
                assert!(!unsaved.exists(), "{case}: staging removed");
                assert!(files_under(&project).iter().all(|name| !name.ends_with("-journal") && !name.ends_with(".tmp")), "{case}: {:?}", files_under(&project));
            }
        }
    }

    #[test]
    fn staged_db_documents_and_their_journals_are_never_byte_copied() {
        use ms_docstore::DocFormat;
        use ms_page_ops::chapter_docs::{bubbles_doc, layers_doc};
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        put(&layers_doc(&project), &serde_json::json!({"committed": true}), DocFormat::Json);
        put(&layers_doc(&unsaved), &serde_json::json!({"staged": true}), DocFormat::Db);
        put(&bubbles_doc(&unsaved), &serde_json::json!([]), DocFormat::Db);
        // A leftover rollback journal of each staged `.db` (a crash mid-write): copied into the
        // committed tree it would be replayed into the committed `.db` of that name.
        fs::write(unsaved.join("layers/layers.db-journal"), b"journal").expect("write");
        fs::write(unsaved.join("translation_bubbles.db-journal"), b"journal").expect("write");
        fs::write(unsaved.join("layers/.layers.db.4242.tmp-journal"), b"journal").expect("write");
        merge_unsaved_into_project(&unsaved, &project, |committed, _| {
            // The manifest reaches the per-page merge untouched, in its own format.
            assert_eq!(ms_docstore::read_value(&layers_doc(committed.parent().expect("chapter"))).expect("read"), Some(serde_json::json!({"committed": true})));
            Ok(())
        })
        .expect("merge");
        // The committed chapter had no bubbles: the new one joins the chapter's format (the
        // committed JSON manifest), not the staging `.db`.
        assert_eq!(files_under(&project), vec!["layers/layers.json", "translation_bubbles.json"]);
    }

    #[test]
    fn missing_committed_bubbles_is_created_in_the_committed_chapter_format() {
        use ms_docstore::DocFormat;
        use ms_page_ops::chapter_docs::{bubbles_doc, layers_doc};
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        put(&layers_doc(&project), &serde_json::json!({"pages": []}), DocFormat::Db);
        put(&bubbles_doc(&unsaved), &serde_json::json!([{"id": 2}]), DocFormat::Json);
        merge_unsaved_into_project(&unsaved, &project, |_, _| Ok(())).expect("merge");
        assert_eq!(files_under(&project), vec!["layers/layers.db", "translation_bubbles.db"]);
        assert_eq!(ms_docstore::read_value(&bubbles_doc(&project)).expect("read"), Some(serde_json::json!([{"id": 2}])));
    }

    #[test]
    fn layers_merge_failure_keeps_staging() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        fs::create_dir_all(unsaved.join("layers")).expect("mkdir");
        fs::write(unsaved.join("layers/layers.json"), b"{}").expect("write");
        merge_unsaved_into_project(&unsaved, &project, |_, _| Err("boom".to_string())).expect_err("must fail");
        assert!(unsaved.join("layers/layers.json").exists());
    }
}
