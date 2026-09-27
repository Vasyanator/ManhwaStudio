/*
File: tests_db.rs

Purpose:
Contract tests of the SQLite fragment codec, format resolution, whole-document writes and
the conversion protocol (native only; every test works in its own temp directory).

Notes:
- Golden fixtures: `crates/ms-docstore/fixtures/<case>.json` + `<case>.rows.json`, shared
  with the Python `docstore.py`. Opt-in cross-language hooks:
  `MS_DOCSTORE_FIXTURE_OUT=<dir>` also writes every case as `<dir>/<case>.db` (read by
  `test_docstore.py` with `MS_DOCSTORE_FIXTURE_IN`); `MS_DOCSTORE_FIXTURE_IN=<dir>` reads
  Python-written `<dir>/<case>.db` files. Both are off by default (test hygiene).
- `set_default_format` is thread-local in this crate's tests (resolve.rs), so a test that
  switches to `Db` never leaks into a parallel one; threads a test spawns see `Json`.
*/

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::sqlite::test_support::{RowTuple, dump_meta, dump_rows, exec, install_write_log, take_write_log};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Every fixture case name (`<case>.rows.json` present), sorted.
fn cases() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixtures_dir())
        .expect("fixtures dir")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.strip_suffix(".rows.json").map(str::to_owned))
        .collect();
    names.sort();
    names
}

fn load_case(case: &str) -> (Value, Vec<RowTuple>) {
    let dir = fixtures_dir();
    let value: Value = serde_json::from_str(&fs::read_to_string(dir.join(format!("{case}.json"))).expect("case json")).expect("parse case");
    let rows: Vec<Value> = serde_json::from_str(&fs::read_to_string(dir.join(format!("{case}.rows.json"))).expect("rows json")).expect("parse rows");
    let text = |row: &Value, key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let rows = rows.iter().map(|row| (text(row, "path").expect("path"), text(row, "parent"), text(row, "seg"), text(row, "kind").expect("kind"), text(row, "payload"))).collect();
    (value, rows)
}

fn tuples(rows: &[split::Row]) -> Vec<RowTuple> {
    rows.iter().map(|row| (row.path.clone(), row.parent.clone(), row.seg.clone(), row.kind.as_str().to_owned(), row.payload.clone())).collect()
}

fn from_tuples(rows: &[RowTuple]) -> Vec<split::Row> {
    rows.iter()
        .map(|(path, parent, seg, kind, payload)| split::Row { path: path.clone(), parent: parent.clone(), seg: seg.clone(), kind: split::FragKind::parse(kind).expect("known kind"), payload: payload.clone() })
        .collect()
}

fn db_doc(dir: &Path, name: &str) -> DocRef {
    DocRef::new(dir.join(name), DocKind::Layers).with_new_format(DocFormat::Db)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).expect("list").map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// `LockedDoc::read_value` as a `with_lock` body (a method path is not general enough).
fn locked_read(locked: &LockedDoc<'_>) -> Result<Option<Value>> {
    locked.read_value()
}

/// `LockedDoc::read_typed_snapshot::<Value>` as a `with_lock` body.
fn locked_typed(locked: &LockedDoc<'_>) -> Result<Option<(Value, Fingerprint)>> {
    locked.read_typed_snapshot()
}

fn opts() -> WriteOptions {
    WriteOptions::default()
}

// ---------------------------------------------------------------------------
// Fixture parity (the contract shared with docstore.py)
// ---------------------------------------------------------------------------

#[test]
fn every_fixture_splits_to_its_rows_and_joins_back() {
    let cases = cases();
    assert!(cases.len() >= 15, "{cases:?}");
    for case in &cases {
        let (value, rows) = load_case(case);
        assert_eq!(tuples(&split::split(&value)), rows, "split({case}) must equal {case}.rows.json byte for byte");
        let mut shuffled = from_tuples(&rows);
        shuffled.reverse();
        assert_eq!(split::join(&shuffled).unwrap_or_else(|err| panic!("join({case}): {err}")), value, "join({case}.rows.json) must equal {case}.json");
    }
}

#[test]
fn every_fixture_round_trips_through_a_database_file() {
    let out_dir = std::env::var_os("MS_DOCSTORE_FIXTURE_OUT").map(PathBuf::from);
    let dir = tempfile::tempdir().expect("temp dir");
    for case in cases() {
        let (value, rows) = load_case(&case);
        let doc = DocRef::new(dir.path().join(&case), DocKind::UserConfig);
        write_whole_atomic(&doc, &value, DocFormat::Db).unwrap_or_else(|err| panic!("write {case}: {err}"));
        let db_path = doc.path_for(DocFormat::Db);
        assert_eq!(actual_format(&doc).expect("format"), Some(DocFormat::Db));
        assert_eq!(read_value(&doc).expect("read"), Some(value.clone()), "{case}: db round trip must be Value-equal");
        assert_eq!(dump_rows(&db_path), rows, "{case}: stored rows");
        let meta = dump_meta(&db_path);
        assert_eq!((meta["schema_version"].as_str(), meta["doc_kind"].as_str(), meta["revision"].as_str(), meta["writer"].as_str()), ("1", "user_config", "0", "rust"));
        if let Some(out_dir) = &out_dir {
            fs::create_dir_all(out_dir).expect("fixture out dir");
            fs::copy(&db_path, out_dir.join(format!("{case}.db"))).expect("export fixture db");
        }
    }
    assert_eq!(names(dir.path()).iter().filter(|name| is_temp_artifact(Path::new(name.as_str()))).count(), 0, "no temp may survive");
}

/// Reverse direction of the cross-language check: databases written by `docstore.py`.
#[test]
fn python_written_databases_read_back_equal_when_provided() {
    let Some(in_dir) = std::env::var_os("MS_DOCSTORE_FIXTURE_IN").map(PathBuf::from) else { return };
    for case in cases() {
        let (value, rows) = load_case(&case);
        let db_path = in_dir.join(format!("{case}.db"));
        assert!(db_path.is_file(), "Python did not write {}", db_path.display());
        assert_eq!(dump_rows(&db_path), rows, "{case}: Python-written rows");
        let doc = DocRef::new(&db_path, DocKind::UserConfig);
        assert_eq!(read_value(&doc).expect("read python db"), Some(value), "{case}: Python-written db must read Value-equal");
    }
}

// ---------------------------------------------------------------------------
// Diff write
// ---------------------------------------------------------------------------

/// A layers-like document: positional pages, `uid`-keyed trees.
fn layers_like(nodes: usize) -> Value {
    let tree: Vec<Value> = (0..nodes).map(|index| json!({"uid": format!("n{index}"), "name": format!("Layer {index}"), "opacity": 1.0, "visible": true, "effects": {"blur": 0.5, "list": [1, 2, 3]}, "bbox": [index, 0, 100, 200]})).collect();
    json!({"version": 3, "pages": [{"name": "001.png", "tree": tree, "groups": []}]})
}

#[test]
fn a_diff_write_touches_only_the_changed_rows_and_bumps_the_revision_once() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "layers");
    let mut value = layers_like(50);
    write_value(&doc, &value, opts()).expect("create");
    let db_path = doc.path_for(DocFormat::Db);
    assert_eq!(revision(&doc).expect("rev"), Some(0));
    let total_rows = dump_rows(&db_path).len();
    install_write_log(&db_path);

    value["pages"][0]["tree"][7]["name"] = json!("renamed");
    write_value(&doc, &value, opts()).expect("diff write");
    assert_eq!(take_write_log(&db_path), vec![("upd".to_owned(), "/pages/0/tree/\"n7\"".to_owned())], "only the changed node row, updated in place");
    assert_eq!(revision(&doc).expect("rev"), Some(1));
    assert_eq!(dump_rows(&db_path).len(), total_rows);

    write_value(&doc, &value, opts()).expect("same write");
    assert!(take_write_log(&db_path).is_empty(), "an unchanged document writes nothing");
    assert_eq!(revision(&doc).expect("rev"), Some(1), "an empty diff keeps the revision");

    update(&doc, opts(), |root| {
        root["pages"][0]["tree"].as_array_mut().ok_or("tree")?.remove(3);
        root["version"] = json!(4);
        Ok(())
    })
    .expect("update");
    let mut log = take_write_log(&db_path);
    log.sort();
    assert_eq!(log, vec![("del".to_owned(), "/pages/0/tree/\"n3\"".to_owned()), ("upd".to_owned(), "/pages/0/tree".to_owned()), ("upd".to_owned(), "/version".to_owned())]);
    assert_eq!(revision(&doc).expect("rev"), Some(2));
    assert_eq!(dump_meta(&db_path)["writer"], "rust");
    assert_eq!(read_value(&doc).expect("read").expect("present")["pages"][0]["tree"].as_array().map(Vec::len), Some(49));
    update(&doc, opts(), |root| {
        root["extra"] = json!(true);
        Ok(())
    })
    .expect("new key");
    assert_eq!(take_write_log(&db_path), vec![("ins".to_owned(), "/extra".to_owned())], "only a NEW path is inserted");
}

#[test]
fn foreign_float_spellings_are_not_rewritten() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "numbers");
    let value = json!({"one": 1.0, "tenth": 0.1, "tiny": 0.00001, "int": 1, "arr": [1.0, 2]});
    write_value(&doc, &value, opts()).expect("create");
    let db_path = doc.path_for(DocFormat::Db);
    // Python spells 1e-5 as `1e-05`; simulate a Python-written row.
    exec(&db_path, "UPDATE frag SET payload = '1e-05' WHERE path = '/tiny'");
    install_write_log(&db_path);
    write_value(&doc, &value, opts()).expect("rewrite same value");
    assert!(take_write_log(&db_path).is_empty(), "text-different but value-equal floats count as unchanged");
    assert_eq!(revision(&doc).expect("rev"), Some(0));
    assert_eq!(read_value(&doc).expect("read"), Some(value.clone()));
    // Type-strictness: 1.0 -> 1 IS a change.
    let mut changed = value;
    changed["one"] = json!(1);
    write_value(&doc, &changed, opts()).expect("int write");
    assert_eq!(take_write_log(&db_path), vec![("upd".to_owned(), "/one".to_owned())]);
}

/// Integer pragma of the database at `path`.
fn pragma(path: &Path, name: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("open");
    conn.pragma_query_value(None, name, |row| row.get(0)).expect("pragma")
}

/// `rowid` of the `frag` row at `row_path`.
fn rowid(path: &Path, row_path: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("open");
    conn.query_row("SELECT rowid FROM frag WHERE path = ?1", [row_path], |row| row.get(0)).expect("rowid")
}

#[test]
fn repeated_one_row_updates_keep_rowids_and_leave_no_free_pages() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "layers");
    let mut value = layers_like(300);
    write_value(&doc, &value, opts()).expect("create");
    let db_path = doc.path_for(DocFormat::Db);
    let row_path = "/pages/0/tree/\"n5\"";
    let first_rowid = rowid(&db_path, row_path);
    let pages_before = pragma(&db_path, "page_count");
    assert_eq!(pragma(&db_path, "freelist_count"), 0, "a fresh file is compact");
    for round in 0..60 {
        // A payload of the same size each round, like a text edit of one node.
        value["pages"][0]["tree"][5]["name"] = json!(format!("Layer {round:04}"));
        write_value(&doc, &value, opts()).expect("one-row write");
    }
    assert_eq!(revision(&doc).expect("rev"), Some(60));
    assert_eq!(rowid(&db_path, row_path), first_rowid, "an UPDATE keeps the row's rowid (REPLACE would move it)");
    assert_eq!(pragma(&db_path, "freelist_count"), 0, "no free-page churn");
    assert_eq!(pragma(&db_path, "page_count"), pages_before, "the file does not grow");
    assert_eq!(read_value(&doc).expect("read"), Some(value));
}

#[test]
fn an_identical_db_write_commits_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "layers");
    let value = layers_like(20);
    write_value(&doc, &value, opts()).expect("create");
    let db_path = doc.path_for(DocFormat::Db);
    let before = fs::metadata(&db_path).expect("stat");
    let signature_before = signature(&doc).expect("signature");
    // Unchecked and Matching baselines alike: an empty diff never touches the file.
    let fingerprint = write_value(&doc, &value, opts()).expect("identical write");
    write_value(&doc, &value, WriteOptions { baseline: SaveBaseline::Matching(fingerprint), ..opts() }).expect("identical write under a baseline");
    let after = fs::metadata(&db_path).expect("stat");
    assert_eq!(revision(&doc).expect("rev"), Some(0), "no revision bump");
    assert_eq!(after.modified().expect("mtime"), before.modified().expect("mtime"), "no commit reached the file");
    assert_eq!(after.len(), before.len());
    assert_eq!(signature(&doc).expect("signature"), signature_before);
    assert!(!crate::whole::journal_path(&db_path).exists(), "no journal was left or needed");
}

// ---------------------------------------------------------------------------
// Strict reads
// ---------------------------------------------------------------------------

fn fresh_db(dir: &Path, name: &str, value: &Value) -> (DocRef, PathBuf) {
    let doc = db_doc(dir, name);
    write_value(&doc, value, opts()).expect("create");
    let path = doc.path_for(DocFormat::Db);
    (doc, path)
}

#[test]
fn contract_violations_in_a_database_are_malformed_and_never_overwritten() {
    let dir = tempfile::tempdir().expect("temp dir");
    let value = json!({"list": [{"id": 1}, {"id": 2}], "n": 1});
    let breakages: [(&str, &str); 5] = [
        ("missing_ent", "DELETE FROM frag WHERE path = '/list/2'"),
        ("unknown_kind", "UPDATE frag SET kind = 'blob' WHERE path = '/n'"),
        ("orphan", "INSERT INTO frag VALUES ('/x/y', '/x', 'y', 'leaf', '1')"),
        ("bad_revision", "UPDATE meta SET value = 'x' WHERE key = 'revision'"),
        ("foreign_app", "PRAGMA application_id = 7"),
    ];
    for (name, sql) in breakages {
        let (doc, path) = fresh_db(dir.path(), name, &value);
        exec(&path, sql);
        let before = fs::read(&path).expect("bytes");
        let err = read_value(&doc).expect_err(name);
        assert!(matches!(err, DocStoreError::Malformed { .. }), "{name}: {err:?}");
        let err = update(&doc, opts(), |root| {
            root["n"] = json!(2);
            Ok(())
        })
        .expect_err(name);
        assert!(matches!(err, DocStoreError::Malformed { .. }), "{name}: {err:?}");
        assert_eq!(fs::read(&path).expect("bytes"), before, "{name}: a malformed database is left untouched");
    }
    let (doc, path) = fresh_db(dir.path(), "newer", &value);
    exec(&path, "PRAGMA user_version = 2");
    assert!(matches!(read_value(&doc), Err(DocStoreError::Unsupported { format: DocFormat::Db, .. })));
}

#[test]
fn a_lone_db_failing_the_header_sniff_is_malformed_not_absent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "layers");
    fs::write(doc.path_for(DocFormat::Db), b"").expect("empty db");
    assert!(matches!(actual_format(&doc), Err(DocStoreError::Malformed { .. })));
    assert!(!exists(&doc));
    assert!(matches!(update(&doc, opts(), |_| Ok(())), Err(DocStoreError::Malformed { .. })));
    assert_eq!(fs::read(doc.path_for(DocFormat::Db)).expect("bytes"), b"", "never replaced by a new document");
}

#[test]
fn opening_resets_a_wal_header_and_leaves_no_sidecars() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (doc, path) = fresh_db(dir.path(), "layers", &json!({"a": 1}));
    exec(&path, "PRAGMA journal_mode = WAL");
    let header = fs::read(&path).expect("bytes");
    assert_eq!((header[18], header[19]), (2, 2), "setup: WAL in the header");
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"a": 1})));
    let header = fs::read(&path).expect("bytes");
    assert_eq!((header[18], header[19]), (1, 1), "a read resets the file to journal_mode=DELETE");
    assert_eq!(names(dir.path()), vec!["layers.db".to_owned()], "no -wal/-shm sidecars");
    exec(&path, "PRAGMA journal_mode = WAL");
    update(&doc, opts(), |root| {
        root["a"] = json!(2);
        Ok(())
    })
    .expect("update");
    let header = fs::read(&path).expect("bytes");
    assert_eq!((header[18], header[19]), (1, 1), "a write resets it too");
    assert_eq!(names(dir.path()), vec!["layers.db".to_owned()]);
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[test]
fn both_formats_equal_are_repaired_by_the_first_locked_operation() {
    let dir = tempfile::tempdir().expect("temp dir");
    set_default_format(DocFormat::Db);
    let doc = DocRef::new(dir.path().join("settings"), DocKind::ProjectSettings);
    write_value(&doc, &json!({"k": 1}), opts()).expect("db");
    fs::write(doc.path_for(DocFormat::Json), "{\"k\": 1}").expect("leftover json");
    assert_eq!(actual_format(&doc).expect("format"), Some(DocFormat::Db), "default format is authoritative");
    assert_eq!(names(dir.path()).len(), 2, "an unlocked read never repairs");
    update(&doc, opts(), |root| {
        root["k"] = json!(2);
        Ok(())
    })
    .expect("update repairs then writes");
    assert_eq!(names(dir.path()), vec!["settings.db".to_owned()]);
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"k": 2})));
}

#[test]
fn both_formats_different_are_ambiguous_and_nothing_is_deleted() {
    let dir = tempfile::tempdir().expect("temp dir");
    set_default_format(DocFormat::Json);
    let doc = DocRef::new(dir.path().join("settings"), DocKind::ProjectSettings);
    write_value(&doc, &json!({"k": 1}), opts()).expect("json");
    write_whole_atomic(&doc, &json!({"k": 99}), DocFormat::Db).expect("db copy");
    assert_eq!(read_value(&doc).expect("unlocked read uses the authoritative json"), Some(json!({"k": 1})));
    let err = update(&doc, opts(), |_| Ok(())).expect_err("ambiguous");
    assert!(matches!(err, DocStoreError::Ambiguous { .. }), "{err:?}");
    let err = with_lock(&doc, locked_read).expect_err("ambiguous in a section too");
    assert!(matches!(err, DocStoreError::Ambiguous { .. }), "{err:?}");
    assert_eq!(names(dir.path()), vec!["settings.db".to_owned(), "settings.json".to_owned()]);
    // A leftover that does not even parse is not deleted either.
    fs::write(doc.path_for(DocFormat::Db), b"garbage").expect("garbage db");
    assert!(matches!(update(&doc, opts(), |_| Ok(())), Err(DocStoreError::Ambiguous { .. })));
    assert_eq!(names(dir.path()).len(), 2);
}

#[test]
fn exactly_one_existing_format_wins_regardless_of_the_default() {
    let dir = tempfile::tempdir().expect("temp dir");
    set_default_format(DocFormat::Db);
    let doc = DocRef::new(dir.path().join("terms"), DocKind::Terms);
    fs::write(doc.path_for(DocFormat::Json), "[]").expect("json");
    update(&doc, opts(), |root| {
        *root = json!([1]);
        Ok(())
    })
    .expect("update");
    assert_eq!(names(dir.path()), vec!["terms.json".to_owned()], "an existing JSON document stays JSON in Db mode");
    let fresh = DocRef::new(dir.path().join("characters"), DocKind::Characters);
    write_value(&fresh, &json!([]), opts()).expect("new");
    assert_eq!(actual_format(&fresh).expect("format"), Some(DocFormat::Db), "a new document takes the default");
}

#[test]
fn a_new_chapter_document_follows_its_siblings_format() {
    let dir = tempfile::tempdir().expect("temp dir");
    set_default_format(DocFormat::Json);
    let bubbles = DocRef::new(dir.path().join("bubbles"), DocKind::Bubbles);
    let layers = DocRef::new(dir.path().join("layers"), DocKind::Layers);
    assert_eq!(chapter_new_format(&[bubbles.clone(), layers.clone()]), DocFormat::Json, "no sibling: the default");
    write_whole_atomic(&bubbles, &json!({"bubbles": []}), DocFormat::Db).expect("db sibling");
    let format = chapter_new_format(&[bubbles.clone(), layers.clone()]);
    assert_eq!(format, DocFormat::Db);
    let layers = layers.with_new_format(format);
    assert_eq!(layers, DocRef::new(dir.path().join("layers"), DocKind::Layers), "the hint is not part of the identity");
    update(&layers, opts(), |root| {
        root["pages"] = json!([]);
        Ok(())
    })
    .expect("create");
    assert_eq!(actual_format(&layers).expect("format"), Some(DocFormat::Db));
}

// ---------------------------------------------------------------------------
// Whole writes, locks, baselines, copies, quarantine
// ---------------------------------------------------------------------------

#[test]
fn write_whole_atomic_replaces_a_db_without_leftovers_and_drops_a_stale_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "bubbles");
    write_whole_atomic(&doc, &json!({"v": 1}), DocFormat::Db).expect("first");
    write_value(&doc, &json!({"v": 2}), opts()).expect("diff write -> revision 1");
    fs::write(dir.path().join("bubbles.db-journal"), b"stale hot journal").expect("journal");
    write_whole_atomic(&doc, &json!({"v": 3}), DocFormat::Db).expect("replace");
    assert_eq!(names(dir.path()), vec!["bubbles.db".to_owned()], "no temp, no journal");
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"v": 3})));
    assert_eq!(revision(&doc).expect("rev"), Some(2), "a whole rewrite keeps the revision monotonic");
    write_whole_atomic(&DocRef::new(dir.path().join("plain"), DocKind::Terms), &json!([1]), DocFormat::Json).expect("json whole");
    assert_eq!(fs::read_to_string(dir.path().join("plain.json")).expect("json"), "[\n  1\n]");
}

#[test]
fn concurrent_updates_of_a_db_document_never_lose_an_increment() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "counter");
    write_value(&doc, &json!({"n": 0}), opts()).expect("create");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let doc = doc.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    update(&doc, opts(), |root| {
                        let next = root["n"].as_u64().ok_or("n")? + 1;
                        root["n"] = json!(next);
                        Ok(())
                    })
                    .expect("update");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("join");
    }
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"n": 200})));
    assert_eq!(revision(&doc).expect("rev"), Some(200));
}

#[test]
fn db_baselines_detect_foreign_changes_and_accept_their_own_snapshot() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "presets");
    let first = write_value(&doc, &json!({"a": 1}), opts()).expect("create");
    let snapshot = read_snapshot(&doc).expect("read").expect("present");
    assert_eq!(snapshot.fingerprint, first, "the write returns the fingerprint a read reports");
    let typed = with_lock(&doc, locked_typed).expect("typed").expect("present");
    assert_eq!(typed.1, first);
    let matching = WriteOptions { baseline: SaveBaseline::Matching(first), ..opts() };
    let second = write_value(&doc, &json!({"a": 2}), matching).expect("baseline matches");
    exec(&doc.path_for(DocFormat::Db), "UPDATE frag SET payload = '3' WHERE path = '/a'");
    let err = write_value(&doc, &json!({"a": 4}), WriteOptions { baseline: SaveBaseline::Matching(second), ..opts() }).expect_err("foreign change");
    match err {
        DocStoreError::Conflict { found, .. } => assert_eq!(Some(found), read_snapshot(&doc).expect("read").map(|snapshot| snapshot.fingerprint)),
        other => panic!("{other:?}"),
    }
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"a": 3})), "nothing written on conflict");
    assert!(matches!(write_value(&doc, &json!({}), WriteOptions { baseline: SaveBaseline::Absent, ..opts() }), Err(DocStoreError::Conflict { .. })));
}

#[test]
fn copy_document_crosses_formats_through_the_value() {
    let dir = tempfile::tempdir().expect("temp dir");
    let staging = DocRef::new(dir.path().join("_unsaved/bubbles"), DocKind::Bubbles);
    write_value(&staging, &json!({"bubbles": [{"id": 1}]}), opts()).expect("staging json");
    let committed = db_doc(dir.path(), "bubbles");
    write_value(&committed, &json!({"bubbles": []}), opts()).expect("committed db");
    assert!(copy_document(&staging, &committed, Durability::Contents).expect("copy"));
    assert_eq!(read_value(&committed).expect("read"), Some(json!({"bubbles": [{"id": 1}]})));
    assert_eq!(names(dir.path()), vec!["_unsaved".to_owned(), "bubbles.db".to_owned()], "the committed document keeps its .db format");
    let back = DocRef::new(dir.path().join("back"), DocKind::Bubbles);
    assert!(copy_document(&committed, &back, Durability::Contents).expect("copy back"));
    assert_eq!(fs::read_to_string(dir.path().join("back.json")).expect("json"), serde_json::to_string_pretty(&json!({"bubbles": [{"id": 1}]})).expect("pretty"));
}

#[test]
fn quarantining_a_db_moves_its_journal_along() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "favorites");
    write_value(&doc, &json!({}), opts()).expect("create");
    exec(&doc.path_for(DocFormat::Db), "PRAGMA application_id = 1");
    fs::write(dir.path().join("favorites.db-journal"), b"journal").expect("journal");
    let moved = quarantine(&doc, "bad", QuarantineNaming::Replace, QuarantineFallback::RenameOnly).expect("quarantine");
    assert_eq!(moved, Quarantined::Moved(dir.path().join("favorites.db.bad")));
    assert_eq!(names(dir.path()), vec!["favorites.db.bad".to_owned(), "favorites.db.bad-journal".to_owned()]);
}

// ---------------------------------------------------------------------------
// Conversion protocol
// ---------------------------------------------------------------------------

/// Records every step; "crashes" after `stop_after`.
struct Failpoint {
    stop_after: Option<ConvertStep>,
    seen: Mutex<Vec<ConvertStep>>,
}

impl ConvertHook for Failpoint {
    fn after_step(&self, _doc: &DocRef, step: ConvertStep) -> Result<(), String> {
        self.seen.lock().expect("seen").push(step);
        if self.stop_after == Some(step) { Err("injected crash".to_owned()) } else { Ok(()) }
    }
}

const STEPS: [ConvertStep; 5] = [ConvertStep::ReadSource, ConvertStep::TempWritten, ConvertStep::TempValidated, ConvertStep::Renamed, ConvertStep::SourceRemoved];

#[test]
fn a_crash_after_any_step_converges_on_rerun_without_loss_or_leftovers() {
    let (value, _) = load_case("layers");
    for (source, target) in [(DocFormat::Json, DocFormat::Db), (DocFormat::Db, DocFormat::Json)] {
        for stop in STEPS {
            let dir = tempfile::tempdir().expect("temp dir");
            // Drivers persist the mode (default format) BEFORE converting.
            set_default_format(target);
            let doc = DocRef::new(dir.path().join("layers"), DocKind::Layers);
            write_whole_atomic(&doc, &value, source).expect("seed source");
            let hook = Failpoint { stop_after: Some(stop), seen: Mutex::new(Vec::new()) };
            let first = convert_document(&doc, target, &hook);
            if stop == ConvertStep::SourceRemoved {
                assert!(first.is_err(), "the hook error surfaces even after the last step");
            } else {
                let err = first.expect_err("interrupted");
                assert!(matches!(&err, DocStoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::Interrupted), "{err:?}");
            }
            // Data is readable at every crash point.
            assert_eq!(read_value(&doc).expect("read after crash"), Some(value.clone()), "{source:?}->{target:?} crash after {stop:?}");
            let rerun = convert_document(&doc, target, &NoHook).expect("rerun");
            let expected = match stop {
                ConvertStep::Renamed => ConvertOutcome::Reconciled,
                ConvertStep::SourceRemoved => ConvertOutcome::Skipped,
                ConvertStep::ReadSource | ConvertStep::TempWritten | ConvertStep::TempValidated => ConvertOutcome::Converted,
            };
            assert_eq!(rerun, expected, "{source:?}->{target:?} after {stop:?}");
            assert_eq!(names(dir.path()), vec![format!("layers.{}", target.extension())], "{source:?}->{target:?} after {stop:?}: only the target file");
            assert_eq!(read_value(&doc).expect("read"), Some(value.clone()));
            assert_eq!(convert_document(&doc, target, &NoHook).expect("idempotent"), ConvertOutcome::Skipped);
        }
    }
}

#[test]
fn a_crash_between_rename_and_delete_is_also_repaired_by_a_plain_update() {
    let dir = tempfile::tempdir().expect("temp dir");
    set_default_format(DocFormat::Db);
    let doc = DocRef::new(dir.path().join("settings"), DocKind::ProjectSettings);
    write_whole_atomic(&doc, &json!({"a": 1}), DocFormat::Json).expect("seed");
    let hook = Failpoint { stop_after: Some(ConvertStep::Renamed), seen: Mutex::new(Vec::new()) };
    assert!(convert_document(&doc, DocFormat::Db, &hook).is_err());
    assert_eq!(*hook.seen.lock().expect("seen"), STEPS[..4].to_vec());
    update(&doc, opts(), |root| {
        root["b"] = json!(2);
        Ok(())
    })
    .expect("update");
    assert_eq!(names(dir.path()), vec!["settings.db".to_owned()]);
    assert_eq!(read_value(&doc).expect("read"), Some(json!({"a": 1, "b": 2})));
}

#[test]
fn convert_many_continues_past_failures_and_reports_progress() {
    let dir = tempfile::tempdir().expect("temp dir");
    let good = DocRef::new(dir.path().join("good"), DocKind::Terms);
    let bad = DocRef::new(dir.path().join("bad"), DocKind::Terms);
    let absent = DocRef::new(dir.path().join("absent"), DocKind::Terms);
    let done = DocRef::new(dir.path().join("done"), DocKind::Terms);
    write_value(&good, &json!([1, 2]), opts()).expect("good");
    fs::write(bad.path_for(DocFormat::Json), "{broken").expect("bad");
    write_whole_atomic(&done, &json!([]), DocFormat::Db).expect("done");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let outcome = convert_many(&[good.clone(), bad.clone(), absent, done], DocFormat::Db, &move |done, total| sink.lock().expect("progress").push((done, total)));
    assert_eq!((outcome.converted, outcome.skipped), (1, 2));
    assert_eq!(outcome.failed.len(), 1);
    assert!(matches!(outcome.failed[0].1, DocStoreError::Malformed { .. }));
    assert_eq!(*seen.lock().expect("progress"), vec![(1, 4), (2, 4), (3, 4), (4, 4)]);
    assert_eq!(read_value(&good).expect("read"), Some(json!([1, 2])));
    assert_eq!(fs::read_to_string(bad.path_for(DocFormat::Json)).expect("bad kept"), "{broken");
}

#[test]
fn the_chapter_format_report_lists_existing_documents_by_tree() {
    let dir = tempfile::tempdir().expect("temp dir");
    let committed = [DocRef::new(dir.path().join("bubbles"), DocKind::Bubbles), DocRef::new(dir.path().join("layers"), DocKind::Layers)];
    let staging = [DocRef::new(dir.path().join("_unsaved/bubbles"), DocKind::Bubbles), DocRef::new(dir.path().join("_unsaved/layers"), DocKind::Layers)];
    write_whole_atomic(&committed[0], &json!({}), DocFormat::Db).expect("db");
    write_whole_atomic(&committed[1], &json!({}), DocFormat::Json).expect("json");
    fs::create_dir_all(dir.path().join("_unsaved")).expect("staging dir");
    fs::write(staging[1].path_for(DocFormat::Db), b"nope").expect("bad staging db");
    let report = chapter_format_report(&committed, &staging);
    assert_eq!(report.committed, vec![(DocKind::Bubbles, DocFormat::Db), (DocKind::Layers, DocFormat::Json)]);
    assert!(report.staging.is_empty());
    assert_eq!(report.unreadable.len(), 1);
    assert!(report.needs_conversion(DocFormat::Db));
}

// ---------------------------------------------------------------------------
// Measurement (run explicitly: `cargo test -p ms-docstore --release -- --ignored --nocapture`)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "latency measurement, not a contract test"]
fn measure_diff_write_latency_on_a_5000_node_document() {
    let dir = tempfile::tempdir().expect("temp dir");
    let doc = db_doc(dir.path(), "layers");
    let mut value = layers_like(5000);
    let started = std::time::Instant::now();
    write_value(&doc, &value, opts()).expect("create");
    let create_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut samples = Vec::new();
    for round in 0..20 {
        value["pages"][0]["tree"][round * 100]["name"] = json!(format!("renamed {round}"));
        let started = std::time::Instant::now();
        write_value(&doc, &value, opts()).expect("diff write");
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let started = std::time::Instant::now();
    let read = read_value(&doc).expect("read");
    let read_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(read, Some(value.clone()));
    let started = std::time::Instant::now();
    let rows = split::split(&value);
    let split_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = std::time::Instant::now();
    let joined = split::join(&rows).expect("join");
    let join_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(joined, value);
    let json_doc = DocRef::new(dir.path().join("layers_json"), DocKind::Layers).with_new_format(DocFormat::Json);
    let mut json_samples = Vec::new();
    for _ in 0..5 {
        let started = std::time::Instant::now();
        write_value(&json_doc, &value, opts()).expect("json write");
        json_samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    json_samples.sort_by(f64::total_cmp);
    println!("pure split {split_ms:.1} ms, pure join {join_ms:.1} ms; same document as JSON (whole atomic write, Contents durability) median {:.1} ms", json_samples[json_samples.len() / 2]);
    let lock_started = std::time::Instant::now();
    for _ in 0..1000 {
        with_lock(&doc, |_| ());
    }
    let lock_us = lock_started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "5000-node layers-like doc ({} rows): create {create_ms:.1} ms; one-node diff write median {:.1} ms, max {:.1} ms; full read {read_ms:.1} ms; lock acquire+normalize {lock_us:.3} us each",
        dump_rows(&doc.path_for(DocFormat::Db)).len(),
        samples[samples.len() / 2],
        samples[samples.len() - 1]
    );
}
