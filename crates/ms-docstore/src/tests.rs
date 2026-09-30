/*
File: tests.rs

Purpose:
Contract tests of the public store API (native only). Every test works inside its own
`tempfile` directory and never touches a real document (PROJECT_RULES.md "Test hygiene").
The atomic-write recipe's own step-order tests live next to it in `json.rs`.
*/

use std::fs;
use std::path::Path;

use serde_json::json;

use super::*;

/// A `DocRef` for `<dir>/<name>` with an arbitrary kind.
fn doc_in(dir: &Path, name: &str) -> DocRef {
    DocRef::new(dir.join(name), DocKind::ProjectSettings)
}

/// File names in `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).expect("list dir").map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn doc_ref_strips_json_and_db_extensions_only() {
    let json = DocRef::new("a/b/user_config.json", DocKind::UserConfig);
    let db = DocRef::new("a/b/user_config.db", DocKind::UserConfig);
    let bare = DocRef::new("a/b/user_config", DocKind::UserConfig);
    assert_eq!(json, bare);
    assert_eq!(db, bare);
    assert_eq!(bare.stem(), Path::new("a/b/user_config"));
    assert_eq!(bare.path_for(DocFormat::Json), Path::new("a/b/user_config.json"));
    assert_eq!(bare.path_for(DocFormat::Db), Path::new("a/b/user_config.db"));
    let other = DocRef::new("a/notes.txt", DocKind::Terms);
    assert_eq!(other.stem(), Path::new("a/notes.txt"), "an unknown extension is part of the stem");
    assert_eq!(other.path_for(DocFormat::Json), Path::new("a/notes.txt.json"));
}

#[test]
fn update_on_an_absent_document_creates_it_with_parent_dirs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(&dir.path().join("nested/title"), "settings");
    assert!(!exists(&doc));
    assert_eq!(read_value(&doc).expect("read absent"), None);
    update(&doc, WriteOptions::default(), |root| {
        root.as_object_mut().ok_or("root must be an object")?.insert("k".into(), json!(1));
        Ok(())
    })
    .expect("update");
    assert!(exists(&doc));
    assert_eq!(actual_format(&doc).expect("format"), Some(DocFormat::Json));
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"k": 1})));
}

#[test]
fn default_layout_matches_to_string_pretty_and_newline_is_opt_in() {
    let dir = tempfile::tempdir().expect("temp dir");
    let value = json!({"b": {"x": [1, 2]}, "a": "é"});
    let plain = doc_in(dir.path(), "plain");
    write_value(&plain, &value, WriteOptions::default()).expect("write");
    let expected = serde_json::to_string_pretty(&value).expect("serialize");
    assert_eq!(fs::read_to_string(plain.path_for(DocFormat::Json)).expect("read"), expected);

    let newline = doc_in(dir.path(), "newline");
    let fp = write_value(&newline, &value, WriteOptions { trailing_newline: true, ..WriteOptions::default() }).expect("write");
    let on_disk = fs::read(newline.path_for(DocFormat::Json)).expect("read");
    assert_eq!(on_disk, format!("{expected}\n").into_bytes());
    assert_eq!(fp, fingerprint(&on_disk), "the returned fingerprint is that of the bytes written");
}

#[test]
fn typed_write_keeps_struct_field_order() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Doc {
        zeta: u32,
        alpha: u32,
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "typed");
    write(&doc, &Doc { zeta: 1, alpha: 2 }, WriteOptions::default()).expect("write");
    let text = fs::read_to_string(doc.path_for(DocFormat::Json)).expect("read");
    assert!(text.find("zeta").expect("zeta") < text.find("alpha").expect("alpha"), "{text}");
    assert_eq!(read::<Doc>(&doc).expect("read"), Some(Doc { zeta: 1, alpha: 2 }));
}

#[test]
fn a_malformed_document_is_never_overwritten_by_update() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "settings");
    let path = doc.path_for(DocFormat::Json);
    fs::write(&path, "{ not json").expect("seed corrupt file");
    let mut mutator_ran = false;
    let err = update(&doc, WriteOptions::default(), |_| {
        mutator_ran = true;
        Ok(())
    })
    .expect_err("a malformed document must fail the update");
    assert!(matches!(err, DocStoreError::Malformed { .. }), "{err:?}");
    assert!(!mutator_ran, "the mutator must not run on a malformed document");
    assert_eq!(fs::read_to_string(&path).expect("read"), "{ not json", "the file must be untouched");
    assert!(matches!(read_value(&doc), Err(DocStoreError::Malformed { .. })));
}

#[test]
fn a_failing_mutator_writes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "settings");
    write_value(&doc, &json!({"keep": true}), WriteOptions::default()).expect("seed");
    let before = fs::read(doc.path_for(DocFormat::Json)).expect("read");
    let err = update(&doc, WriteOptions::default(), |root| -> Result<(), String> {
        root["keep"] = json!(false);
        Err("refused".into())
    })
    .expect_err("the mutator error must surface");
    assert!(matches!(err, DocStoreError::Mutator(ref cause) if cause == "refused"), "{err:?}");
    assert_eq!(fs::read(doc.path_for(DocFormat::Json)).expect("read"), before);
}

#[test]
fn an_atomic_write_leaves_no_temp_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "settings");
    write_value(&doc, &json!({"a": 1}), WriteOptions::default()).expect("first");
    update(&doc, WriteOptions { durability: Durability::ContentsAndDirectory, ..WriteOptions::default() }, |root| {
        root["a"] = json!(2);
        Ok(())
    })
    .expect("second");
    assert_eq!(entries(dir.path()), vec!["settings.json".to_owned()]);
    assert_eq!(recorded_steps(&doc.path_for(DocFormat::Json)).last(), Some(&WriteStep::DirectoryDurable));
}

#[test]
fn concurrent_updates_never_lose_an_increment() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 25;
    let dir = tempfile::tempdir().expect("temp dir");
    // Two spellings of the same document must share one lock.
    let plain = doc_in(dir.path(), "counter");
    let dotted = DocRef::new(dir.path().join(".").join("counter.json"), DocKind::ProjectSettings);
    std::thread::scope(|scope| {
        for index in 0..THREADS {
            let doc = if index % 2 == 0 { &plain } else { &dotted };
            scope.spawn(move || {
                for _ in 0..ROUNDS {
                    update(doc, WriteOptions::default(), |root| {
                        let current = root.get("n").and_then(Value::as_u64).unwrap_or(0);
                        root["n"] = json!(current + 1);
                        Ok(())
                    })
                    .expect("update");
                }
            });
        }
    });
    let total = u64::try_from(THREADS * ROUNDS).expect("small");
    assert_eq!(read_value(&plain).expect("read"), Some(json!({"n": total})));
}

#[test]
fn with_lock_allows_read_then_write_without_relocking() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "manifest");
    write_value(&doc, &json!({"pages": []}), WriteOptions::default()).expect("seed");
    let result = with_lock(&doc, |locked| -> Result<Fingerprint> {
        let mut value = locked.read_value()?.expect("present");
        value["pages"] = json!(["p1"]);
        locked.write_value(&value, WriteOptions::default())
    });
    let written = result.expect("locked write");
    assert_eq!(read_snapshot(&doc).expect("read").expect("present").fingerprint, written);
}

#[test]
fn baseline_conflict_is_detected_and_nothing_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "presets");
    let opts = WriteOptions { trailing_newline: true, ..WriteOptions::default() };

    // Absent file + `Absent` baseline: allowed. Absent + `Matching`: also allowed.
    let mine = write_value(&doc, &json!({"v": 1}), WriteOptions { baseline: SaveBaseline::Absent, ..opts }).expect("create");
    // Matching baseline: allowed, returns the next baseline.
    let next = write_value(&doc, &json!({"v": 2}), WriteOptions { baseline: SaveBaseline::Matching(mine), ..opts }).expect("matching write");

    // Another instance replaces the file behind our back.
    fs::write(doc.path_for(DocFormat::Json), "{\"v\": 99}\n").expect("foreign write");
    let err = write_value(&doc, &json!({"v": 3}), WriteOptions { baseline: SaveBaseline::Matching(next), ..opts }).expect_err("stale baseline");
    let DocStoreError::Conflict { found, .. } = err else { panic!("expected Conflict, got {err:?}") };
    assert_eq!(found, fingerprint(b"{\"v\": 99}\n"));
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"v": 99})), "a conflict must not write");

    // `Absent` never accepts an existing file.
    let err = write_value(&doc, &json!({"v": 4}), WriteOptions { baseline: SaveBaseline::Absent, ..opts }).expect_err("absent expected");
    assert!(matches!(err, DocStoreError::Conflict { .. }), "{err:?}");

    // A vanished file never blocks.
    remove(&doc).expect("remove");
    write_value(&doc, &json!({"v": 5}), WriteOptions { baseline: SaveBaseline::Matching(next), ..opts }).expect("vanished file never blocks");
}

#[test]
fn signature_changes_after_a_write_and_is_none_when_absent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "characters");
    assert_eq!(signature(&doc).expect("absent"), None);
    write_value(&doc, &json!({"a": 1}), WriteOptions::default()).expect("first");
    let first = signature(&doc).expect("sig").expect("present");
    write_value(&doc, &json!({"a": 1, "bb": 22}), WriteOptions::default()).expect("second");
    let second = signature(&doc).expect("sig").expect("present");
    assert_ne!(first, second);
}

#[test]
fn copy_document_is_byte_identical_and_reports_an_absent_source() {
    let dir = tempfile::tempdir().expect("temp dir");
    let staging = DocRef::new(dir.path().join("_unsaved/bubbles.json"), DocKind::Bubbles);
    let committed = DocRef::new(dir.path().join("chapter/bubbles.json"), DocKind::Bubbles);
    assert!(!copy_document(&staging, &committed, Durability::Contents).expect("absent source"));
    assert!(!exists(&committed), "an absent source writes nothing");

    fs::create_dir_all(dir.path().join("_unsaved")).expect("mkdir");
    let raw = "{\n    \"bubbles\": [ {\"id\": 1} ]\n}\n";
    fs::write(staging.path_for(DocFormat::Json), raw).expect("seed staging");
    assert!(copy_document(&staging, &committed, Durability::ContentsAndDirectory).expect("copy"));
    assert_eq!(fs::read_to_string(committed.path_for(DocFormat::Json)).expect("read"), raw);

    fs::write(staging.path_for(DocFormat::Json), "garbage").expect("corrupt staging");
    let err = copy_document(&staging, &committed, Durability::Contents).expect_err("malformed source");
    assert!(matches!(err, DocStoreError::Malformed { .. }), "{err:?}");
    assert_eq!(fs::read_to_string(committed.path_for(DocFormat::Json)).expect("read"), raw, "the destination must be untouched");
}

#[test]
fn remove_is_idempotent_and_deletes_every_format_and_the_db_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "terms");
    remove(&doc).expect("absent remove");
    write_value(&doc, &json!([]), WriteOptions::default()).expect("write");
    fs::write(doc.path_for(DocFormat::Db), b"stray").expect("stray db");
    fs::write(dir.path().join("terms.db-journal"), b"stale").expect("stray journal");
    assert_eq!(actual_format(&doc).expect("format"), Some(DocFormat::Json), "both exist: the default format is authoritative");
    remove(&doc).expect("remove");
    assert!(!exists(&doc));
    assert!(entries(dir.path()).is_empty(), "every format and the journal are gone: {:?}", entries(dir.path()));
}

#[test]
fn the_process_default_starts_as_json() {
    assert_eq!(default_format(), DocFormat::Json);
}

/// `LockedDoc::read_typed_snapshot` as a value, for `with_lock`.
fn typed_snapshot(locked: &LockedDoc<'_>) -> Result<Option<(Value, Fingerprint)>> {
    locked.read_typed_snapshot()
}

#[test]
fn typed_snapshot_fingerprints_the_bytes_it_parsed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "presets");
    let written = write_value(&doc, &json!({"a": [1, 2]}), WriteOptions::default()).expect("write");
    let (value, fingerprint) = with_lock(&doc, typed_snapshot).expect("read").expect("present");
    assert_eq!(value, json!({"a": [1, 2]}));
    assert_eq!(fingerprint, written, "the fingerprint must describe the same bytes as the value");
    // A baseline taken from it accepts exactly that state.
    let opts = WriteOptions { baseline: SaveBaseline::Matching(fingerprint), ..WriteOptions::default() };
    write_value(&doc, &json!({"a": 3}), opts).expect("matching baseline accepts");
    assert!(matches!(write_value(&doc, &json!({}), opts), Err(DocStoreError::Conflict { .. })));
    assert_eq!(with_lock(&doc, typed_snapshot).expect("read absent-safe").map(|(value, _)| value), Some(json!({"a": 3})));
    fs::write(doc.path_for(DocFormat::Json), "{broken").expect("corrupt");
    assert!(matches!(with_lock(&doc, typed_snapshot), Err(DocStoreError::Malformed { .. })));
}

#[test]
fn a_locked_section_sees_its_own_first_write_of_a_new_document() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "terms");
    with_lock(&doc, |locked| {
        assert_eq!(locked.actual_format().expect("format"), None);
        assert_eq!(locked.signature().expect("signature"), None);
        locked.write_value(&json!({"k": 1}), WriteOptions::default()).expect("write");
        assert_eq!(locked.actual_format().expect("format"), Some(DocFormat::Json));
        assert!(locked.signature().expect("signature").is_some());
        assert_eq!(locked.read_value().expect("read"), Some(json!({"k": 1})));
        locked.remove().expect("remove");
        assert_eq!(locked.read_value().expect("read"), None);
    });
    assert!(!exists(&doc));
}

#[test]
fn quarantine_replace_renames_over_an_older_copy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "fonts_data");
    assert_eq!(quarantine(&doc, "bad", QuarantineNaming::Replace, QuarantineFallback::CopyIfRenameFails).expect("absent"), Quarantined::Absent);
    let path = doc.path_for(DocFormat::Json);
    let bad = dir.path().join("fonts_data.json.bad");
    fs::write(&bad, "older").expect("older quarantine");
    fs::write(&path, "{corrupt").expect("corrupt");
    assert_eq!(quarantine(&doc, "bad", QuarantineNaming::Replace, QuarantineFallback::CopyIfRenameFails).expect("quarantine"), Quarantined::Moved(bad.clone()));
    assert!(!path.exists());
    assert_eq!(fs::read_to_string(&bad).expect("read bad"), "{corrupt");
}

#[test]
fn quarantine_first_free_never_overwrites_and_gives_up_when_full() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "char_favorites");
    let path = doc.path_for(DocFormat::Json);
    fs::write(dir.path().join("char_favorites.json.bad"), "first").expect("earlier copy");
    fs::write(&path, "[broken").expect("corrupt");
    let naming = QuarantineNaming::FirstFree { max_candidates: 2 };
    let second = dir.path().join("char_favorites.json.bad.1");
    assert_eq!(quarantine(&doc, "bad", naming, QuarantineFallback::RenameOnly).expect("quarantine"), Quarantined::Moved(second.clone()));
    assert_eq!(fs::read_to_string(dir.path().join("char_favorites.json.bad")).expect("first kept"), "first");
    assert_eq!(fs::read_to_string(&second).expect("second"), "[broken");

    fs::write(&path, "[again").expect("corrupt again");
    let err = quarantine(&doc, "bad", naming, QuarantineFallback::RenameOnly).expect_err("no free name left");
    assert!(matches!(err, DocStoreError::Quarantine { .. }), "{err:?}");
    assert_eq!(fs::read_to_string(&path).expect("untouched"), "[again", "a failed quarantine leaves the document in place");
}

#[test]
fn quarantine_rename_only_failure_is_reported_and_copy_fallback_is_used_when_allowed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "presets");
    let path = doc.path_for(DocFormat::Json);
    fs::write(&path, "{corrupt").expect("corrupt");
    // A NON-EMPTY directory at the destination: rename cannot replace it, and copy cannot
    // write a file there either.
    let bad = dir.path().join("presets.json.bad");
    fs::create_dir(&bad).expect("blocking dir");
    fs::write(bad.join("keep"), "x").expect("fill dir");
    let err = quarantine(&doc, "bad", QuarantineNaming::Replace, QuarantineFallback::RenameOnly).expect_err("rename must fail");
    assert!(matches!(err, DocStoreError::Quarantine { .. }), "{err:?}");
    let err = quarantine(&doc, "bad", QuarantineNaming::Replace, QuarantineFallback::CopyIfRenameFails).expect_err("copy must fail too");
    assert!(matches!(&err, DocStoreError::Quarantine { copy_error: Some(_), .. }), "{err:?}");
    assert!(err.to_string().contains("copy failed"), "{err}");
    assert_eq!(fs::read_to_string(&path).expect("untouched"), "{corrupt");
}

#[test]
fn unsynced_writes_are_still_atomic_documents() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = doc_in(dir.path(), "settings");
    let opts = WriteOptions { durability: Durability::None, ..WriteOptions::default() };
    update(&doc, opts, |root| {
        root.as_object_mut().ok_or("object")?.insert("comic_type".into(), json!("pages"));
        Ok(())
    })
    .expect("update");
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"comic_type": "pages"})));
    assert_eq!(entries(dir.path()), vec!["settings.json".to_owned()]);
}
