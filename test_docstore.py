"""
File: test_docstore.py

Purpose:
Contract tests of `docstore.py` (the Python half of ms-docstore) and of its users
`config.BaseUserConfig` and the AI-backend device-selection writers.

Main responsibilities:
- fixture parity with `crates/ms-docstore/fixtures` (split == rows, join(rows) == json);
- `.db` round trip, row-level diff write, revision semantics, BEGIN IMMEDIATE serialization;
- atomic `.json` update, never-create rule, both-exist resolution, strict join errors;
- `config.py` never writes on import and never overwrites an unreadable document;
- opt-in cross-language check of Rust-written databases (`MS_DOCSTORE_FIXTURE_IN=<dir>`).

Notes:
Run with `venv/bin/python -m pytest test_docstore.py`. Every test works in a temp dir;
the repository's real user_config is never touched.
"""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

import pytest

import docstore
from docstore import DocStoreError, ErrorKind, Format, Row

REPO_ROOT = Path(__file__).resolve().parent
FIXTURES = REPO_ROOT / "crates" / "ms-docstore" / "fixtures"
CASES = sorted(path.name[: -len(".rows.json")] for path in FIXTURES.glob("*.rows.json"))


def load_case(case: str) -> tuple[Any, list[Row]]:
    value = docstore.parse_json((FIXTURES / f"{case}.json").read_text(encoding="utf-8"))
    rows = [Row.from_dict(item) for item in json.loads((FIXTURES / f"{case}.rows.json").read_text(encoding="utf-8"))]
    return value, rows


def make_db(tmp_path: Path, value: Any, name: str = "doc") -> Path:
    stem = tmp_path / name
    docstore._create_database(docstore.path_for(stem, Format.DB), value, doc_kind="user_config")
    return stem


def make_json(tmp_path: Path, value: Any, name: str = "doc") -> Path:
    stem = tmp_path / name
    docstore.path_for(stem, Format.JSON).write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")
    return stem


def db_rows(path: Path) -> list[Row]:
    conn = sqlite3.connect(str(path))
    try:
        return sorted((Row(*record) for record in conn.execute("SELECT path, parent, seg, kind, payload FROM frag")), key=lambda row: row.path)
    finally:
        conn.close()


def db_meta(path: Path) -> dict[str, str]:
    conn = sqlite3.connect(str(path))
    try:
        return dict(conn.execute("SELECT key, value FROM meta").fetchall())
    finally:
        conn.close()


def install_write_log(path: Path) -> None:
    """Adds triggers that record every row INSERT/UPDATE/DELETE (`ins`/`upd`/`del`) on `frag`."""
    conn = sqlite3.connect(str(path), isolation_level=None)
    try:
        conn.execute("CREATE TABLE write_log(op TEXT, path TEXT)")
        conn.execute("CREATE TRIGGER log_ins AFTER INSERT ON frag BEGIN INSERT INTO write_log VALUES ('ins', NEW.path); END")
        conn.execute("CREATE TRIGGER log_upd AFTER UPDATE ON frag BEGIN INSERT INTO write_log VALUES ('upd', NEW.path); END")
        conn.execute("CREATE TRIGGER log_del AFTER DELETE ON frag BEGIN INSERT INTO write_log VALUES ('del', OLD.path); END")
    finally:
        conn.close()


def take_write_log(path: Path) -> list[tuple[str, str]]:
    conn = sqlite3.connect(str(path), isolation_level=None)
    try:
        entries = sorted(conn.execute("SELECT op, path FROM write_log").fetchall())
        conn.execute("DELETE FROM write_log")
        return entries
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Fixture parity
# ---------------------------------------------------------------------------


def test_fixtures_present() -> None:
    assert len(CASES) >= 15, CASES


@pytest.mark.parametrize("case", CASES)
def test_fixture_split_matches_rows(case: str) -> None:
    value, rows = load_case(case)
    assert docstore.split(value) == rows


@pytest.mark.parametrize("case", CASES)
def test_fixture_join_matches_json(case: str) -> None:
    value, rows = load_case(case)
    assert docstore.values_equal(docstore.join(list(reversed(rows))), value)


@pytest.mark.parametrize("case", CASES)
def test_db_round_trip(case: str, tmp_path: Path) -> None:
    value, rows = load_case(case)
    stem = make_db(tmp_path, value)
    db_path = docstore.path_for(stem, Format.DB)
    assert docstore.resolve(stem) is Format.DB
    assert docstore.values_equal(docstore.read_document(stem), value)
    assert db_rows(db_path) == rows
    assert docstore.read_revision(db_path) == 0


@pytest.mark.skipif(not os.environ.get("MS_DOCSTORE_FIXTURE_IN"), reason="MS_DOCSTORE_FIXTURE_IN not set (Rust-written databases unavailable)")
@pytest.mark.parametrize("case", CASES)
def test_rust_written_database(case: str) -> None:
    db_path = Path(os.environ["MS_DOCSTORE_FIXTURE_IN"]) / f"{case}.db"
    assert db_path.is_file(), f"Rust did not write {db_path}"
    value, rows = load_case(case)
    assert docstore.is_sqlite_file(db_path)
    assert docstore.values_equal(docstore._read_db(db_path), value)
    assert db_rows(db_path) == rows
    meta = db_meta(db_path)
    assert meta["schema_version"] == "1" and meta["revision"].isdigit()


# ---------------------------------------------------------------------------
# split / join edge cases
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("value", [{"a": 2**64}, {"a": -(2**63) - 1}, {"a": float("nan")}, {"a": float("inf")}, {1: "x"}, {"a": (1, 2)}, {"a": "\ud800"}])
def test_split_rejects_unrepresentable_values(value: Any) -> None:
    with pytest.raises(DocStoreError) as info:
        docstore.split(value)
    assert info.value.kind is ErrorKind.UNSUPPORTED


def test_values_equal_is_type_strict() -> None:
    assert not docstore.values_equal(1, 1.0)
    assert not docstore.values_equal(True, 1)
    assert not docstore.values_equal({"a": 1}, {"a": True})
    assert docstore.values_equal(-0.0, 0.0)
    assert docstore.values_equal({"a": [1, None]}, {"a": [1, None]})


def _rows(*items: tuple[str, Any, Any, str, Any]) -> list[Row]:
    return [Row(*item) for item in items]


@pytest.mark.parametrize(
    "rows",
    [
        pytest.param([], id="no-root"),
        pytest.param(_rows(("", None, None, "obj", None), ("x", None, None, "obj", None)), id="two-roots"),
        pytest.param(_rows(("/r", None, None, "obj", None)), id="root-path-not-empty"),
        pytest.param(_rows(("", None, None, "obj", None), ("/a", "/missing", "a", "leaf", "1")), id="orphan"),
        pytest.param(_rows(("", None, None, "leaf", "1"), ("/a", "", "a", "leaf", "1")), id="child-of-leaf"),
        pytest.param(_rows(("", None, None, "arr", "2"), ("/0", "", "0", "leaf", "1")), id="arr-gap"),
        pytest.param(_rows(("", None, None, "arr", "1"), ("/0", "", "0", "leaf", "1"), ("/1", "", "1", "leaf", "1")), id="arr-extra"),
        pytest.param(_rows(("", None, None, "arr", "x"),), id="arr-bad-count"),
        pytest.param(_rows(("", None, None, "arr_id", '["a","b"]'), ('/"a"', "", '"a"', "ent", '{"id":"a"}')), id="arr_id-missing"),
        pytest.param(_rows(("", None, None, "arr_id", '["a"]'), ('/"a"', "", '"a"', "ent", '{"id":"a"}'), ('/"b"', "", '"b"', "ent", '{"id":"b"}')), id="arr_id-extra"),
        pytest.param(_rows(("", None, None, "obj", "{}"),), id="obj-payload"),
        pytest.param(_rows(("", None, None, "weird", None),), id="unknown-kind"),
        pytest.param(_rows(("", None, None, "leaf", "{bad"),), id="bad-payload"),
        pytest.param(_rows(("", None, None, "leaf", "NaN"),), id="nan-payload"),
        pytest.param(_rows(("", None, None, "obj", None), ("/a", "", "a", "leaf", "1"), ("/b", "", "a", "leaf", "2")), id="duplicate-seg"),
    ],
)
def test_join_rejects_malformed_rows(rows: list[Row]) -> None:
    with pytest.raises(DocStoreError) as info:
        docstore.join(rows)
    assert info.value.kind is ErrorKind.MALFORMED


def test_stem_of_strips_one_known_extension() -> None:
    assert docstore.stem_of(Path("a/user_config.json")) == Path("a/user_config")
    assert docstore.stem_of(Path("a/user_config.db")) == Path("a/user_config")
    assert docstore.stem_of(Path("a/x.y")) == Path("a/x.y")
    assert docstore.path_for(Path("a/b.c"), Format.DB) == Path("a/b.c.db")


# ---------------------------------------------------------------------------
# .db diff write
# ---------------------------------------------------------------------------


def test_diff_write_touches_only_changed_rows(tmp_path: Path) -> None:
    value, _rows_unused = load_case("user_config_like")
    stem = make_db(tmp_path, value)
    db_path = docstore.path_for(stem, Format.DB)
    install_write_log(db_path)

    docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("General", "ai_device"), "cpu"))
    assert take_write_log(db_path) == [("upd", "/General/ai_device")]
    assert db_meta(db_path)["revision"] == "1"
    assert db_meta(db_path)["writer"] == "python"

    # An empty diff commits nothing and leaves the revision alone.
    docstore.update_document(stem, lambda doc: None)
    docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("General", "ai_device"), "cpu"))
    assert take_write_log(db_path) == []
    assert db_meta(db_path)["revision"] == "1"

    # Removing a subtree deletes exactly its rows; adding one inserts exactly its rows.
    docstore.update_document(stem, lambda doc: doc["TextTab"].pop("params"))
    assert take_write_log(db_path) == [("del", "/TextTab/params"), ("del", "/TextTab/params/empty"), ("del", "/TextTab/params/font size multiplier")]
    docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("New", "k"), [{"id": 1}]))
    assert take_write_log(db_path) == [("ins", "/New"), ("ins", "/New/k"), ("ins", "/New/k/1")]
    assert db_meta(db_path)["revision"] == "3"
    expected = json.loads(json.dumps(value))
    expected["General"]["ai_device"] = "cpu"
    del expected["TextTab"]["params"]
    expected["New"] = {"k": [{"id": 1}]}
    assert docstore.values_equal(docstore.read_document(stem), expected)


def test_diff_write_keeps_foreign_float_lexeme(tmp_path: Path) -> None:
    """A float Rust spelled differently (value-equal) is not rewritten by an unrelated update."""
    stem = make_db(tmp_path, {"f": 1e-05, "g": 1})
    db_path = docstore.path_for(stem, Format.DB)
    conn = sqlite3.connect(str(db_path))
    with conn:
        conn.execute("UPDATE frag SET payload = '0.00001' WHERE path = '/f'")
    conn.close()
    install_write_log(db_path)
    docstore.update_document(stem, lambda doc: doc.__setitem__("g", 2))
    assert take_write_log(db_path) == [("upd", "/g")]
    assert {row.path: row.payload for row in db_rows(db_path)}["/f"] == "0.00001"
    assert docstore.read_document(stem) == {"f": 1e-05, "g": 2}


def test_repeated_one_row_updates_keep_rowids_and_leave_no_free_pages(tmp_path: Path) -> None:
    """Changed rows are UPDATEd in place: rowid kept, no free-page churn, no growth."""
    tree = [{"uid": f"n{index}", "name": f"Layer {index}", "opacity": 1.0, "bbox": [index, 0, 100, 200]} for index in range(300)]
    stem = make_db(tmp_path, {"version": 3, "pages": [{"name": "001.png", "tree": tree}]})
    db_path = docstore.path_for(stem, Format.DB)

    def pragma(name: str) -> int:
        conn = sqlite3.connect(str(db_path))
        try:
            return int(conn.execute(f"PRAGMA {name}").fetchone()[0])
        finally:
            conn.close()

    def rowid(row_path: str) -> int:
        conn = sqlite3.connect(str(db_path))
        try:
            return int(conn.execute("SELECT rowid FROM frag WHERE path = ?", (row_path,)).fetchone()[0])
        finally:
            conn.close()

    row_path = '/pages/0/tree/"n5"'
    first_rowid = rowid(row_path)
    pages_before = pragma("page_count")
    assert pragma("freelist_count") == 0
    for round_index in range(60):
        docstore.update_document(stem, lambda doc, r=round_index: doc["pages"][0]["tree"][5].__setitem__("name", f"Layer {r:04d}"))
    assert db_meta(db_path)["revision"] == "60"
    assert rowid(row_path) == first_rowid
    assert pragma("freelist_count") == 0
    assert pragma("page_count") == pages_before


def test_mutator_failure_writes_nothing(tmp_path: Path) -> None:
    stem = make_db(tmp_path, {"a": 1})

    def failing(doc: dict) -> None:
        doc["a"] = 2
        raise RuntimeError("boom")

    with pytest.raises(DocStoreError) as info:
        docstore.update_document(stem, failing)
    assert info.value.kind is ErrorKind.MUTATOR
    assert docstore.read_document(stem) == {"a": 1}
    assert db_meta(docstore.path_for(stem, Format.DB))["revision"] == "0"


def test_write_resets_wal_journal_mode(tmp_path: Path) -> None:
    stem = make_db(tmp_path, {"a": 1})
    db_path = docstore.path_for(stem, Format.DB)
    conn = sqlite3.connect(str(db_path))
    assert conn.execute("PRAGMA journal_mode=WAL").fetchone()[0] == "wal"
    conn.close()
    docstore.update_document(stem, lambda doc: doc.__setitem__("a", 2))
    conn = sqlite3.connect(str(db_path))
    assert conn.execute("PRAGMA journal_mode").fetchone()[0] == "delete"
    conn.close()
    assert not Path(f"{db_path}-wal").exists()


def test_foreign_database_is_rejected(tmp_path: Path) -> None:
    db_path = tmp_path / "doc.db"
    conn = sqlite3.connect(str(db_path))
    conn.execute("CREATE TABLE t(x)")
    conn.commit()
    conn.close()
    with pytest.raises(DocStoreError) as info:
        docstore.read_document(tmp_path / "doc")
    assert info.value.kind is ErrorKind.SCHEMA


def test_begin_immediate_serializes_writers(tmp_path: Path) -> None:
    """A second writer waits for the first transaction and then re-reads: no lost update."""
    stem = make_db(tmp_path, {"General": {"theme": "dark", "ai_device": "cuda"}})
    db_path = docstore.path_for(stem, Format.DB)
    holder = sqlite3.connect(str(db_path), isolation_level=None)
    holder.execute("BEGIN IMMEDIATE")
    errors: list[BaseException] = []

    def writer() -> None:
        try:
            docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("General", "ai_device"), "cpu"))
        except BaseException as exc:  # surfaced by the assertion below
            errors.append(exc)

    thread = threading.Thread(target=writer)
    thread.start()
    time.sleep(0.4)
    assert thread.is_alive(), "the Python writer must wait for the held RESERVED lock"
    holder.execute("UPDATE frag SET payload = '\"light\"' WHERE path = '/General/theme'")
    holder.execute("UPDATE meta SET value = '1' WHERE key = 'revision'")
    holder.execute("COMMIT")
    holder.close()
    thread.join(timeout=10)
    assert not thread.is_alive() and not errors, errors
    assert docstore.read_document(stem) == {"General": {"theme": "light", "ai_device": "cpu"}}
    assert db_meta(db_path)["revision"] == "2"


# ---------------------------------------------------------------------------
# .json codec
# ---------------------------------------------------------------------------


def test_json_update_is_atomic_and_leaves_no_temp(tmp_path: Path) -> None:
    stem = make_json(tmp_path, {"General": {"theme": "dark"}, "keep": [1, 2.0]})
    docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("General", "ai_device"), "cpu"))
    assert docstore.read_document(stem) == {"General": {"theme": "dark", "ai_device": "cpu"}, "keep": [1, 2.0]}
    assert isinstance(docstore.read_document(stem)["keep"][1], float)
    assert sorted(path.name for path in tmp_path.iterdir()) == ["doc.json"]


def test_json_update_failure_keeps_old_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    stem = make_json(tmp_path, {"a": 1})
    before = docstore.path_for(stem, Format.JSON).read_bytes()

    def broken_replace(src: Any, dst: Any) -> None:
        raise OSError("disk full")

    monkeypatch.setattr(docstore.os, "replace", broken_replace)
    with pytest.raises(DocStoreError) as info:
        docstore.update_document(stem, lambda doc: doc.__setitem__("a", 2))
    assert info.value.kind is ErrorKind.IO
    assert docstore.path_for(stem, Format.JSON).read_bytes() == before
    assert sorted(path.name for path in tmp_path.iterdir()) == ["doc.json"]


def test_json_unchanged_update_does_not_write(tmp_path: Path) -> None:
    stem = make_json(tmp_path, {"a": 1})
    path = docstore.path_for(stem, Format.JSON)
    before = path.read_bytes()
    docstore.update_document(stem, lambda doc: doc.__setitem__("a", 1))
    assert path.read_bytes() == before


def test_malformed_json_is_never_overwritten(tmp_path: Path) -> None:
    path = tmp_path / "doc.json"
    path.write_text("{broken", encoding="utf-8")
    with pytest.raises(DocStoreError) as info:
        docstore.update_document(tmp_path / "doc", lambda doc: doc.__setitem__("a", 1))
    assert info.value.kind is ErrorKind.MALFORMED
    assert path.read_text(encoding="utf-8") == "{broken"


# ---------------------------------------------------------------------------
# Resolution: never create, lone bad .db, both-exist rule
# ---------------------------------------------------------------------------


def test_never_creates_a_document(tmp_path: Path) -> None:
    stem = tmp_path / "doc"
    assert docstore.resolve(stem) is None
    assert docstore.read_document(stem) is None
    with pytest.raises(DocStoreError) as info:
        docstore.update_document(stem, lambda doc: doc.__setitem__("a", 1))
    assert info.value.kind is ErrorKind.NOT_FOUND
    assert list(tmp_path.iterdir()) == []


def test_lone_db_failing_the_sniff_is_malformed(tmp_path: Path) -> None:
    (tmp_path / "doc.db").write_bytes(b"")
    with pytest.raises(DocStoreError) as info:
        docstore.resolve(tmp_path / "doc")
    assert info.value.kind is ErrorKind.MALFORMED


@pytest.mark.parametrize(
    ("mode", "expected"),
    [("dev", Format.JSON), ("prod", Format.DB), (None, Format.DB)],
)
def test_both_exist_prefers_the_mode_file(tmp_path: Path, mode: Any, expected: Format) -> None:
    general: dict[str, Any] = {} if mode is None else {"storage_mode": mode}
    make_db(tmp_path, {"General": dict(general), "from": "db"})
    make_json(tmp_path, {"General": dict(general), "from": "json"})
    stem = tmp_path / "doc"
    assert docstore.resolve(stem) is expected
    assert docstore.read_document(stem)["from"] == expected.value
    with pytest.raises(DocStoreError) as info:
        docstore.update_document(stem, lambda doc: doc.__setitem__("x", 1))
    assert info.value.kind is ErrorKind.RECONCILE_PENDING
    assert sorted(path.name for path in tmp_path.iterdir()) == ["doc.db", "doc.json"]


@pytest.mark.parametrize(
    ("mode", "expected"),
    [(" DEV ", Format.JSON), ("Prod", Format.DB), ("bogus", Format.DB), (3, Format.DB)],
)
def test_storage_mode_spelling_matches_rust(mode: Any, expected: Format) -> None:
    # Rust `StorageMode::from_config_str` trims and ignores ASCII case; unknown -> prod.
    assert docstore.storage_mode_format({"General": {"storage_mode": mode}}) is expected


def test_reader_rolls_back_a_hot_journal(tmp_path: Path) -> None:
    # Simulates a writer that crashed mid-transaction: snapshot the database AND its
    # `-journal` while a write transaction has spilled pages to the file, then read the
    # snapshot. A `mode=ro` reader fails there; a `mode=rw` reader rolls the journal back.
    original = {"General": {"storage_mode": "prod"}, "items": [{"id": n, "text": "x" * 64} for n in range(200)]}
    stem = make_db(tmp_path, original)
    live = docstore.path_for(stem, Format.DB)
    crashed_dir = tmp_path / "crashed"
    crashed_dir.mkdir()
    crashed = crashed_dir / live.name
    conn = sqlite3.connect(live, isolation_level=None)
    try:
        conn.execute("PRAGMA journal_mode=DELETE").fetchall()
        conn.execute("PRAGMA cache_size=1")
        conn.execute("BEGIN IMMEDIATE")
        conn.execute("DELETE FROM frag")
        conn.execute("INSERT INTO frag (path, parent, seg, kind, payload) SELECT 'junk' || n, NULL, NULL, 'x', zeroblob(4096) FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 64) SELECT n FROM c)")
        journal = live.with_name(live.name + "-journal")
        assert journal.is_file() and journal.stat().st_size > 0
        crashed.write_bytes(live.read_bytes())
        crashed.with_name(crashed.name + "-journal").write_bytes(journal.read_bytes())
        conn.execute("ROLLBACK")
    finally:
        conn.close()
    assert docstore.read_document(crashed_dir / "doc") == original
    assert not crashed.with_name(crashed.name + "-journal").exists()


def test_both_exist_mode_from_the_only_parsable_file(tmp_path: Path) -> None:
    make_db(tmp_path, {"General": {"storage_mode": "prod"}})
    (tmp_path / "doc.json").write_text("{broken", encoding="utf-8")
    assert docstore.resolve(tmp_path / "doc") is Format.DB


def test_both_exist_mode_file_unreadable_is_an_error(tmp_path: Path) -> None:
    make_json(tmp_path, {"General": {"storage_mode": "prod"}})
    (tmp_path / "doc.db").write_bytes(b"not sqlite at all")
    with pytest.raises(DocStoreError) as info:
        docstore.read_document(tmp_path / "doc")
    assert info.value.kind is ErrorKind.MALFORMED


def test_both_exist_disagreeing_modes_are_ambiguous(tmp_path: Path) -> None:
    make_db(tmp_path, {"General": {"storage_mode": "prod"}})
    make_json(tmp_path, {"General": {"storage_mode": "dev"}})
    with pytest.raises(DocStoreError) as info:
        docstore.resolve(tmp_path / "doc")
    assert info.value.kind is ErrorKind.AMBIGUOUS


# ---------------------------------------------------------------------------
# config.py and the backend writers
# ---------------------------------------------------------------------------

import config  # noqa: E402  (after docstore tests: importing config creates model folders)


DEFAULTS = {"General": {"theme": "dark", "ai_device": "not-selected", "nested": {"x": 1}}}


def test_config_reads_db_and_merges_defaults_in_memory_only(tmp_path: Path) -> None:
    stem = make_db(tmp_path, {"General": {"theme": "light"}}, name="user_config")
    db_path = docstore.path_for(stem, Format.DB)
    cfg = config.BaseUserConfig(os.fspath(tmp_path / "user_config.json"), DEFAULTS)
    assert cfg.General.theme == "light"
    assert cfg.General.nested.x == 1
    assert docstore.read_document(stem) == {"General": {"theme": "light"}}
    assert db_meta(db_path)["revision"] == "0"


def test_config_parse_error_keeps_defaults_and_file(tmp_path: Path) -> None:
    path = tmp_path / "user_config.json"
    path.write_text("{not json", encoding="utf-8")
    cfg = config.BaseUserConfig(os.fspath(path), DEFAULTS)
    assert cfg.config == DEFAULTS
    assert cfg.config["General"] is not DEFAULTS["General"]
    assert path.read_text(encoding="utf-8") == "{not json"


def test_config_missing_document_is_not_created(tmp_path: Path) -> None:
    cfg = config.BaseUserConfig(os.fspath(tmp_path / "user_config.json"), DEFAULTS)
    assert cfg.General.theme == "dark"
    with pytest.raises(DocStoreError):
        cfg.General.theme = "light"
    assert cfg.General.theme == "light", "the session keeps the choice in memory"
    assert list(tmp_path.iterdir()) == []


@pytest.mark.parametrize("fmt", [Format.DB, Format.JSON])
def test_config_update_rereads_and_touches_only_its_key(tmp_path: Path, fmt: Format) -> None:
    initial = {"General": {"theme": "dark"}}
    stem = make_db(tmp_path, initial, name="user_config") if fmt is Format.DB else make_json(tmp_path, initial, name="user_config")
    cfg = config.BaseUserConfig(os.fspath(tmp_path / "user_config.json"), DEFAULTS)
    # Another writer (the Rust app) changes a different key after the snapshot was taken.
    docstore.update_document(stem, lambda doc: docstore.set_path(doc, ("General", "theme"), "light"))
    cfg.General.ai_device = "cpu"
    assert docstore.read_document(stem) == {"General": {"theme": "light", "ai_device": "cpu"}}
    assert cfg.General.theme == "light" and cfg.General.ai_device == "cpu"
    cfg.TopLevel = 5
    assert docstore.read_document(stem)["TopLevel"] == 5


def test_import_config_does_not_write(tmp_path: Path) -> None:
    """Importing config.py (module-level UserConfig) must not create or rewrite user_config."""
    code = "import config; print(config.UserConfig.General.theme)"
    env = dict(os.environ, PYTHONPATH=os.fspath(REPO_ROOT))
    result = subprocess.run([sys.executable, "-c", code], cwd=tmp_path, env=env, capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, result.stderr
    assert list(tmp_path.iterdir()) == [], "a missing user_config must not be created on import"

    path = tmp_path / "user_config.json"
    path.write_text('{"General": {"theme": "light"}}', encoding="utf-8")
    before = (path.read_bytes(), path.stat().st_mtime_ns)
    result = subprocess.run([sys.executable, "-c", code], cwd=tmp_path, env=env, capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "light"
    assert (path.read_bytes(), path.stat().st_mtime_ns) == before


def test_ai_device_writer_sets_only_its_keys(tmp_path: Path) -> None:
    from modules.ai_device import AIDevice

    stem = make_db(tmp_path, {"General": {"theme": "dark"}, "Other": {"k": 1}}, name="user_config")
    cfg = config.BaseUserConfig(os.fspath(tmp_path / "user_config.json"), DEFAULTS)
    AIDevice._set_config_value(cfg, "cpu")
    assert docstore.read_document(stem) == {"General": {"theme": "dark", "ai_device": "cpu", "ai_device_configured": True}, "Other": {"k": 1}}


def test_device_service_writers_set_only_their_keys(tmp_path: Path) -> None:
    from modules.ai_backend.runtime.device_service import AiDeviceService, _OnnxDeviceSelector

    stem = make_db(tmp_path, {"General": {"theme": "dark", "ai_max_loaded_models": 3}}, name="user_config")
    cfg = config.BaseUserConfig(os.fspath(tmp_path / "user_config.json"), DEFAULTS)
    selector = _OnnxDeviceSelector(cfg)
    selector._set_config_value(selector.PROVIDER_CONFIG_PATH, "CPUExecutionProvider", mark_configured=True)
    assert docstore.read_document(stem)["General"] == {"theme": "dark", "ai_max_loaded_models": 3, "ai_onnx_provider": "CPUExecutionProvider", "ai_onnx_provider_configured": True}

    class FakeManager:
        def __init__(self) -> None:
            self.limit = 0

        def set_max_loaded_models(self, value: int) -> None:
            self.limit = value

        def get_max_loaded_models(self) -> int:
            return self.limit

    service = AiDeviceService.__new__(AiDeviceService)
    service._user_config = cfg
    service._model_manager = FakeManager()
    revision = docstore.read_revision(docstore.path_for(stem, Format.DB))
    service._ensure_model_limit_config_locked()
    assert docstore.read_revision(docstore.path_for(stem, Format.DB)) == revision, "an unchanged limit must not be rewritten"
    service._set_config_value(AiDeviceService.MAX_LOADED_MODELS_CONFIG_PATH, "5")
    assert docstore.read_document(stem)["General"]["ai_max_loaded_models"] == "5"
