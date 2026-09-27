"""
File: docstore.py

Purpose:
Python half of the ManhwaStudio document store (`crates/ms-docstore` is the Rust half).
A logical document lives at a STEM (a path without extension) either as `<stem>.json`
(Dev mode) or as `<stem>.db` (Prod mode, SQLite). The Python AI backend only touches
`user_config`, and reads/writes it through this module in whichever format exists.

Main responsibilities:
- resolve which file holds a document (`resolve`), including the both-exist rule;
- read a whole document as a JSON value (`read_document`);
- serialized read-modify-write of an EXISTING document (`update_document`):
  `.db` inside one `BEGIN IMMEDIATE` transaction with a row-level diff write,
  `.json` via temp file + fsync + `os.replace`;
- the pure tree <-> rows codec shared byte-for-byte with Rust (`split`, `join`).

Key structures:
- Format: `json` | `db`.
- Row: one `frag` table row (path, parent, seg, kind, payload).
- DocStoreError / ErrorKind: every failure, typed.

Key functions:
- resolve(), read_document(), update_document(), split(), join().

Notes:
- Stdlib only (`sqlite3`, `json`, `pathlib`, `logging`). The backend must not import any
  project module from here (config.py imports this file).
- This module NEVER creates a document and NEVER deletes a file: Rust creates user_config
  before the backend starts and owns every conversion and leftover deletion.
- Golden fixtures shared with Rust: `crates/ms-docstore/fixtures/` (see its README.md).

SPEC CLARIFICATIONS (binding for both implementations; plan-result.md section C was
ambiguous on these points, the literal reading was chosen):
 1. Root row: path "" , parent NULL, seg NULL. Children of the root have parent "" (the
    root's path, NOT NULL) and path "/" + esc(seg).
 2. `seg` stores the RAW segment text (unescaped object key, decimal index "0","1",... for
    positional children, JSON text of the key value for `ent` rows). `path` =
    parent path + "/" + esc(seg), esc = RFC 6901 (`~` -> `~0` first, then `/` -> `~1`).
    An object key "" gives path parent + "/".
 3. `ent` rows: seg = compact JSON text of the key value (`"a1"` with quotes, `12`), and
    their path uses that same text escaped, e.g. `/tree/"a1"`, `/bubbles/12`.
 4. Compact JSON = json.dumps(v, ensure_ascii=False, separators=(",", ":"),
    sort_keys=True, allow_nan=False) == serde_json::to_string on a BTreeMap Value.
    Keys sort by code point (== UTF-8 byte order). Applies to every payload.
 5. Payloads per kind: obj -> NULL; arr -> element count as decimal text; arr_id ->
    compact JSON array of the key values in element order; ent -> compact JSON of the
    whole element (key field included); leaf -> compact JSON of the value.
 6. Array classification: id-keyed iff non-empty, every element an object, key name =
    "id" if the FIRST element has a field "id", else "uid" if it has "uid", else not
    keyed; every element must carry that key with a JSON string or an integer (a bool or
    a float such as 1.0 is NOT an integer); key texts pairwise distinct (compact JSON
    text, so 12 != "12"). Otherwise: no element is an object/array (this includes the
    EMPTY array) -> one `leaf` row with the whole array; else `arr` positional.
 7. Numbers: an integer must fit i64/u64 ([-2**63, 2**64-1]); larger ints and NaN/Inf
    are rejected with DocStoreError(UNSUPPORTED) on write (serde_json would turn them into
    f64 / reject them). Integer lexemes are byte-identical in both languages. Float
    lexemes are byte-identical only for 0 and 1e-4 <= |x| < 1e16 (both print shortest
    round-trip digits in plain decimal there; outside it Python prints `1e-05`/`1e+16`,
    while serde_json 1.0.150 (zmij 1.0.21) prints plain decimal for 1e-5 <= |x| < 1e16 and
    its own exponent form elsewhere, e.g. 1e-5 is `1e-05` vs `0.00001`). RULE:
    fixtures only use floats inside that window, and a payload is "unchanged" in the
    diff write when the text is equal OR both texts parse to TYPE-STRICT equal values
    (int stays distinct from float, bool from int) - so neither side rewrites rows only
    because the other side formats a float differently.
 8. Join is strict: exactly one row with parent NULL and it has path ""; obj payload
    must be NULL; arr children must be exactly segs "0".."n-1"; arr_id children must be
    exactly the key texts listed in its payload (missing, extra or duplicate -> Malformed);
    rows unreachable from the root (orphans, children of leaf/ent) -> Malformed; unknown
    kind -> Malformed. Decoding never parses `path`.
 9. Diff write compares (parent, seg, kind, payload) per path (payload with rule 7);
    changed -> UPDATE in place (rowid kept; never INSERT OR REPLACE, which churns free
    pages), new -> INSERT, vanished -> DELETE. Non-empty diff: meta.revision
    += 1 and meta.writer = 'python' (Rust writes 'rust') in the same transaction. Empty
    diff: nothing written, revision unchanged. doc_kind/app_version/schema_version are
    never touched by a Python write.
10. Every open checks PRAGMA application_id == 0x4D534453, PRAGMA user_version == 1 and
    meta.schema_version == '1'; a missing/unparsable meta.revision is Malformed.
    Write connections set journal_mode=DELETE, synchronous=FULL, foreign_keys=OFF,
    busy_timeout=5000 before BEGIN IMMEDIATE; read connections are opened `mode=rw`
    (never `rwc`, so nothing is created; rw lets SQLite roll back a hot journal) with
    busy_timeout=5000 and read rows + meta in one transaction.
11. `.db` sniff: the first 16 bytes must be b"SQLite format 3\\0". A lone `.db` failing
    the sniff is Malformed (never "absent").
12. Both `.json` and `.db` exist: the mode is `General.storage_mode` ("prod" -> db,
    "dev" -> json, trimmed and case-insensitive; missing -> prod; any other value ->
    warning + prod) read from
    whichever file parses; if both parse with different modes -> AMBIGUOUS; if the
    mode-matching file does not parse -> its Malformed error (no silent fallback).
    Reads use the mode-matching file. `update_document` REFUSES to write while both
    exist (ErrorKind.RECONCILE_PENDING): a Python write into either copy would be lost
    or turn Rust's both-exist repair into `Ambiguous`. It triggers only inside a live
    conversion window; the earlier part of that window is NOT covered (known_gaps KG-016).
13. Dev-mode JSON files are written pretty (indent=2, sort_keys, ensure_ascii=False, no
    trailing newline) through `.{name}.{pid}.tmp` + fsync + os.replace (+ directory fsync
    on POSIX). Byte parity with Rust's pretty JSON is NOT a contract; Value equality is.
"""

from __future__ import annotations

import copy
import enum
import json
import logging
import os
import sqlite3
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

log = logging.getLogger(__name__)

SQLITE_HEADER = b"SQLite format 3\x00"
APPLICATION_ID = 0x4D534453  # 'MSDS'
SCHEMA_VERSION = 1
BUSY_TIMEOUT_MS = 5000
WRITER_NAME = "python"
STORAGE_MODE_SECTION = "General"
STORAGE_MODE_KEY = "storage_mode"
_I64_MIN = -(2**63)
_U64_MAX = 2**64 - 1

KIND_OBJ = "obj"
KIND_ARR = "arr"
KIND_ARR_ID = "arr_id"
KIND_ENT = "ent"
KIND_LEAF = "leaf"
_ALL_KINDS = frozenset({KIND_OBJ, KIND_ARR, KIND_ARR_ID, KIND_ENT, KIND_LEAF})

_SCHEMA_SQL = (
    "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);"
    "CREATE TABLE frag(path TEXT PRIMARY KEY, parent TEXT, seg TEXT, kind TEXT NOT NULL, payload TEXT);"
    "CREATE INDEX frag_parent ON frag(parent);"
)


class Format(str, enum.Enum):
    """On-disk format of a logical document; the value is the file extension."""

    JSON = "json"
    DB = "db"


class ErrorKind(enum.Enum):
    """Failure classes of the document store (mirror of Rust `DocStoreError` variants)."""

    NOT_FOUND = "not_found"  # the document does not exist; nothing is ever created
    IO = "io"  # filesystem error
    MALFORMED = "malformed"  # the file exists but does not parse / violates the row contract
    SCHEMA = "schema"  # a `.db` of a foreign or newer schema
    SQLITE = "sqlite"  # SQLite engine error (locked past busy_timeout, I/O, ...)
    AMBIGUOUS = "ambiguous"  # both formats parse and disagree on the storage mode
    RECONCILE_PENDING = "reconcile_pending"  # both formats exist; writes refused until Rust repairs
    UNSUPPORTED = "unsupported"  # value not representable (huge int, NaN, non-str key, ...)
    MUTATOR = "mutator"  # the caller's mutator raised; nothing was written


class DocStoreError(Exception):
    """A typed document-store failure. `kind` says what failed, `path` which file (if any)."""

    def __init__(self, kind: ErrorKind, message: str, path: Optional[Path] = None) -> None:
        super().__init__(f"{message} (path: {path})" if path is not None else message)
        self.kind = kind
        self.path = path
        self.message = message


@dataclass(frozen=True)
class Row:
    """One `frag` row. `parent`/`seg` are None only for the root row; `payload` None = SQL NULL."""

    path: str
    parent: Optional[str]
    seg: Optional[str]
    kind: str
    payload: Optional[str]

    def to_dict(self) -> dict[str, Optional[str]]:
        """The row as the fixture object `{path, parent, seg, kind, payload}`."""
        return {"path": self.path, "parent": self.parent, "seg": self.seg, "kind": self.kind, "payload": self.payload}

    @staticmethod
    def from_dict(data: dict[str, Any]) -> "Row":
        """Parses a fixture row object; raises DocStoreError(MALFORMED) on a bad shape."""
        try:
            row = Row(data["path"], data["parent"], data["seg"], data["kind"], data["payload"])
        except (KeyError, TypeError) as exc:
            raise DocStoreError(ErrorKind.MALFORMED, f"bad row object {data!r}: {exc}") from exc
        for field_value in (row.parent, row.seg, row.payload):
            if field_value is not None and not isinstance(field_value, str):
                raise DocStoreError(ErrorKind.MALFORMED, f"row field is not text/null: {data!r}")
        if not isinstance(row.path, str) or not isinstance(row.kind, str):
            raise DocStoreError(ErrorKind.MALFORMED, f"row path/kind is not text: {data!r}")
        return row


# ---------------------------------------------------------------------------
# JSON value helpers
# ---------------------------------------------------------------------------


def _reject_constant(name: str) -> Any:
    raise ValueError(f"non-standard JSON constant {name!r} (NaN/Infinity are not JSON)")


def parse_json(text: str) -> Any:
    """Parses strict JSON (NaN/Infinity rejected, like serde_json). Raises ValueError."""
    return json.loads(text, parse_constant=_reject_constant)


def compact_json(value: Any) -> str:
    """Compact, key-sorted JSON text of `value` (spec clarification 4). Raises DocStoreError(UNSUPPORTED)."""
    _validate_value(value)
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True, allow_nan=False)


def _validate_value(value: Any) -> None:
    """Rejects anything serde_json could not round-trip exactly (clarification 7)."""
    stack: list[Any] = [value]
    while stack:
        item = stack.pop()
        if item is None or isinstance(item, bool):
            continue
        if isinstance(item, int):
            if not _I64_MIN <= item <= _U64_MAX:
                raise DocStoreError(ErrorKind.UNSUPPORTED, f"integer {item} is outside the i64/u64 range")
        elif isinstance(item, float):
            if item != item or item in (float("inf"), float("-inf")):
                raise DocStoreError(ErrorKind.UNSUPPORTED, f"float {item!r} is not representable in JSON")
        elif isinstance(item, str):
            try:
                item.encode("utf-8")
            except UnicodeEncodeError as exc:
                raise DocStoreError(ErrorKind.UNSUPPORTED, f"string is not valid Unicode (lone surrogate?): {exc}") from exc
        elif isinstance(item, list):
            stack.extend(item)
        elif isinstance(item, dict):
            for key, nested in item.items():
                if not isinstance(key, str):
                    raise DocStoreError(ErrorKind.UNSUPPORTED, f"object key {key!r} is not a string")
                stack.append(key)
                stack.append(nested)
        else:
            raise DocStoreError(ErrorKind.UNSUPPORTED, f"value of type {type(item).__name__} is not a JSON value")


def values_equal(left: Any, right: Any) -> bool:
    """Type-strict JSON value equality: int != float, bool != int, -0.0 == 0.0 (as serde_json)."""
    if isinstance(left, bool) or isinstance(right, bool):
        return isinstance(left, bool) and isinstance(right, bool) and left == right
    if isinstance(left, int) and isinstance(right, int):
        return left == right
    if isinstance(left, float) and isinstance(right, float):
        return left == right
    if isinstance(left, str) and isinstance(right, str):
        return left == right
    if left is None or right is None:
        return left is None and right is None
    if isinstance(left, list) and isinstance(right, list):
        return len(left) == len(right) and all(values_equal(a, b) for a, b in zip(left, right))
    if isinstance(left, dict) and isinstance(right, dict):
        return left.keys() == right.keys() and all(values_equal(left[key], right[key]) for key in left)
    return False


def _payload_equal(kind: str, old: Optional[str], new: Optional[str]) -> bool:
    """Whether a stored payload already represents `new` (clarification 7: text or strict value equality)."""
    if old == new:
        return True
    if old is None or new is None or kind not in (KIND_LEAF, KIND_ENT, KIND_ARR_ID):
        return False
    try:
        return values_equal(parse_json(old), parse_json(new))
    except ValueError:
        return False


# ---------------------------------------------------------------------------
# split / join (pure, no I/O) — must match crates/ms-docstore byte-for-byte
# ---------------------------------------------------------------------------


def escape_segment(seg: str) -> str:
    """RFC 6901 escaping of one path segment: `~` -> `~0`, then `/` -> `~1`."""
    return seg.replace("~", "~0").replace("/", "~1")


def _id_key_name(array: list[Any]) -> Optional[str]:
    """The key field of an id-keyed array (clarification 6), or None when the array is not keyed."""
    if not array or not all(isinstance(element, dict) for element in array):
        return None
    first = array[0]
    if "id" in first:
        name = "id"
    elif "uid" in first:
        name = "uid"
    else:
        return None
    texts: set[str] = set()
    for element in array:
        if name not in element:
            return None
        key_value = element[name]
        is_integer = isinstance(key_value, int) and not isinstance(key_value, bool)
        if not (isinstance(key_value, str) or is_integer):
            return None
        texts.add(compact_json(key_value))
    return name if len(texts) == len(array) else None


def _split_into(value: Any, path: str, parent: Optional[str], seg: Optional[str], out: list[Row]) -> None:
    if isinstance(value, dict):
        out.append(Row(path, parent, seg, KIND_OBJ, None))
        for key, nested in value.items():
            _split_into(nested, f"{path}/{escape_segment(key)}", path, key, out)
        return
    if isinstance(value, list):
        key_name = _id_key_name(value)
        if key_name is not None:
            order = [element[key_name] for element in value]
            out.append(Row(path, parent, seg, KIND_ARR_ID, compact_json(order)))
            for element in value:
                key_text = compact_json(element[key_name])
                out.append(Row(f"{path}/{escape_segment(key_text)}", path, key_text, KIND_ENT, compact_json(element)))
            return
        if not any(isinstance(element, (dict, list)) for element in value):
            out.append(Row(path, parent, seg, KIND_LEAF, compact_json(value)))
            return
        out.append(Row(path, parent, seg, KIND_ARR, str(len(value))))
        for index, element in enumerate(value):
            _split_into(element, f"{path}/{index}", path, str(index), out)
        return
    out.append(Row(path, parent, seg, KIND_LEAF, compact_json(value)))


def split(value: Any) -> list[Row]:
    """Splits a JSON value into `frag` rows sorted by path (spec section C).

    Raises DocStoreError(UNSUPPORTED) for values serde_json cannot round-trip exactly.
    """
    _validate_value(value)
    rows: list[Row] = []
    _split_into(value, "", None, None, rows)
    rows.sort(key=lambda row: row.path)
    return rows


def _malformed(message: str, path: Optional[Path] = None) -> DocStoreError:
    return DocStoreError(ErrorKind.MALFORMED, message, path)


def _parse_payload(row: Row, source: Optional[Path]) -> Any:
    if row.payload is None:
        raise _malformed(f"row {row.path!r} of kind {row.kind!r} has a NULL payload", source)
    try:
        return parse_json(row.payload)
    except ValueError as exc:
        raise _malformed(f"row {row.path!r} payload is not JSON: {exc}", source) from exc


def join(rows: list[Row], source: Optional[Path] = None) -> Any:
    """Rebuilds the JSON value from `frag` rows (spec section C, clarification 8).

    `source` only labels errors. Raises DocStoreError(MALFORMED) on any contract violation.
    """
    roots = [row for row in rows if row.parent is None]
    if len(roots) != 1 or roots[0].path != "":
        raise _malformed(f"expected exactly one root row with path '' and NULL parent, found {len(roots)}", source)
    children: dict[str, list[Row]] = {}
    for row in rows:
        if row.kind not in _ALL_KINDS:
            raise _malformed(f"row {row.path!r} has unknown kind {row.kind!r}", source)
        if row.parent is not None:
            if row.seg is None:
                raise _malformed(f"row {row.path!r} has a parent but NULL seg", source)
            children.setdefault(row.parent, []).append(row)

    visited = 0

    def by_seg(parent_row: Row) -> dict[str, Row]:
        found: dict[str, Row] = {}
        for child in children.get(parent_row.path, []):
            assert child.seg is not None  # guaranteed by the loop above
            if child.seg in found:
                raise _malformed(f"row {parent_row.path!r} has two children with seg {child.seg!r}", source)
            found[child.seg] = child
        return found

    def build(row: Row) -> Any:
        nonlocal visited
        visited += 1
        if row.kind == KIND_OBJ:
            if row.payload is not None:
                raise _malformed(f"obj row {row.path!r} has a non-NULL payload", source)
            return {seg: build(child) for seg, child in by_seg(row).items()}
        if row.kind == KIND_ARR:
            count_text = row.payload or ""
            if not count_text.isascii() or not count_text.isdigit():
                raise _malformed(f"arr row {row.path!r} has a bad count {row.payload!r}", source)
            count = int(count_text)
            found = by_seg(row)
            expected = [str(index) for index in range(count)]
            if sorted(found) != sorted(expected):
                raise _malformed(f"arr row {row.path!r} children do not match 0..{count}", source)
            return [build(found[seg]) for seg in expected]
        if row.kind == KIND_ARR_ID:
            order = _parse_payload(row, source)
            if not isinstance(order, list):
                raise _malformed(f"arr_id row {row.path!r} payload is not an array", source)
            found = by_seg(row)
            try:
                key_texts = [compact_json(key_value) for key_value in order]
            except DocStoreError as exc:
                raise _malformed(f"arr_id row {row.path!r} has an unrepresentable key: {exc}", source) from exc
            if len(set(key_texts)) != len(key_texts) or set(key_texts) != set(found):
                raise _malformed(f"arr_id row {row.path!r} children do not match its key order", source)
            return [build(found[text]) for text in key_texts]
        # ent / leaf: a whole value in the payload; any child row stays unvisited -> orphan error.
        return _parse_payload(row, source)

    value = build(roots[0])
    if visited != len(rows):
        raise _malformed(f"{len(rows) - visited} row(s) are unreachable from the root", source)
    return value


# ---------------------------------------------------------------------------
# Paths, resolution
# ---------------------------------------------------------------------------


def path_for(stem: Path, fmt: Format) -> Path:
    """The file of `stem` in `fmt` (`<stem>.json` / `<stem>.db`); dots in the stem are kept."""
    return stem.with_name(f"{stem.name}.{fmt.value}")


def stem_of(path: Path) -> Path:
    """Strips one trailing `.json` / `.db` from `path` (mirror of Rust `DocRef::new`)."""
    for fmt in Format:
        suffix = f".{fmt.value}"
        if path.name.endswith(suffix) and len(path.name) > len(suffix):
            return path.with_name(path.name[: -len(suffix)])
    return path


def is_sqlite_file(path: Path) -> bool:
    """Whether `path` starts with the 16-byte SQLite header. Raises DocStoreError(IO) on read errors."""
    try:
        with path.open("rb") as handle:
            return handle.read(len(SQLITE_HEADER)) == SQLITE_HEADER
    except OSError as exc:
        raise DocStoreError(ErrorKind.IO, f"cannot read header: {exc}", path) from exc


def storage_mode_format(document: Any) -> Format:
    """The format `General.storage_mode` selects ("prod" -> db, "dev" -> json, missing/unknown -> db).

    Spelling is trimmed and case-insensitive, like the Rust parser."""
    mode: Any = None
    if isinstance(document, dict):
        section = document.get(STORAGE_MODE_SECTION)
        if isinstance(section, dict):
            mode = section.get(STORAGE_MODE_KEY)
    if mode is None:
        return Format.DB
    # Same spelling rule as Rust `StorageMode::from_config_str`: trimmed, ASCII case-insensitive.
    normalized = mode.strip().lower() if isinstance(mode, str) else None
    if normalized == "prod":
        return Format.DB
    if normalized == "dev":
        return Format.JSON
    log.warning("docstore: unknown %s.%s=%r, treating as 'prod'", STORAGE_MODE_SECTION, STORAGE_MODE_KEY, mode)
    return Format.DB


@dataclass
class _Located:
    fmt: Format
    path: Path
    both_exist: bool
    value: Any = None
    has_value: bool = False


def _exists(path: Path) -> bool:
    try:
        return path.is_file()
    except OSError as exc:
        raise DocStoreError(ErrorKind.IO, f"cannot stat: {exc}", path) from exc


def _locate(stem: Path) -> Optional[_Located]:
    """Resolution of spec section B / clarifications 11-12. None = the document does not exist."""
    json_path = path_for(stem, Format.JSON)
    db_path = path_for(stem, Format.DB)
    json_exists = _exists(json_path)
    db_exists = _exists(db_path)
    if not json_exists and not db_exists:
        return None
    if json_exists and not db_exists:
        return _Located(Format.JSON, json_path, False)
    if db_exists and not json_exists:
        if not is_sqlite_file(db_path):
            raise _malformed("file is not an SQLite database (header sniff failed)", db_path)
        return _Located(Format.DB, db_path, False)

    parsed: dict[Format, Any] = {}
    errors: dict[Format, DocStoreError] = {}
    for fmt, path in ((Format.DB, db_path), (Format.JSON, json_path)):
        try:
            parsed[fmt] = _read_file(fmt, path)
        except DocStoreError as exc:
            errors[fmt] = exc
    if not parsed:
        raise _malformed(f"both {json_path.name} and {db_path.name} exist and neither parses: {errors[Format.DB]}; {errors[Format.JSON]}", stem)
    modes = {fmt: storage_mode_format(value) for fmt, value in parsed.items()}
    if len(set(modes.values())) > 1:
        raise DocStoreError(ErrorKind.AMBIGUOUS, f"{json_path.name} and {db_path.name} both exist and disagree on {STORAGE_MODE_SECTION}.{STORAGE_MODE_KEY}", stem)
    preferred = next(iter(modes.values()))
    if preferred not in parsed:
        raise errors[preferred]
    log.warning("docstore: both %s and %s exist; using %s until the application reconciles them", json_path, db_path, preferred.value)
    return _Located(preferred, path_for(stem, preferred), True, parsed[preferred], True)


def resolve(stem: Path) -> Optional[Format]:
    """The format the document at `stem` is read from, or None when neither file exists.

    Raises DocStoreError (MALFORMED / AMBIGUOUS / IO) per clarifications 11-12.
    """
    located = _locate(Path(stem))
    return located.fmt if located is not None else None


# ---------------------------------------------------------------------------
# SQLite codec
# ---------------------------------------------------------------------------


def _sqlite_error(exc: sqlite3.Error, path: Path, operation: str) -> DocStoreError:
    return DocStoreError(ErrorKind.SQLITE, f"SQLite {operation} failed: {exc}", path)


def _connect(path: Path, *, write: bool) -> sqlite3.Connection:
    """Opens an existing `.db` (never creates it) and checks the schema (clarification 10)."""
    # Readers open `mode=rw` too (never `rwc`: an absent file is still an error, nothing is
    # created): a `mode=ro` connection cannot roll back a hot `-journal` left by a crashed
    # writer and fails with "attempt to write a readonly database". SQLite degrades `rw` to
    # read-only by itself when the OS denies write access. Mirrors the Rust readers.
    uri = f"{path.resolve().as_uri()}?mode=rw"
    try:
        conn = sqlite3.connect(uri, uri=True, timeout=BUSY_TIMEOUT_MS / 1000, isolation_level=None)
    except sqlite3.Error as exc:
        raise _sqlite_error(exc, path, "open") from exc
    try:
        conn.execute(f"PRAGMA busy_timeout={BUSY_TIMEOUT_MS}")
        if write:
            conn.execute("PRAGMA journal_mode=DELETE").fetchall()
            conn.execute("PRAGMA synchronous=FULL")
            conn.execute("PRAGMA foreign_keys=OFF")
        app_id = conn.execute("PRAGMA application_id").fetchone()[0]
        user_version = conn.execute("PRAGMA user_version").fetchone()[0]
    except sqlite3.Error as exc:
        conn.close()
        raise _sqlite_error(exc, path, "configure") from exc
    if app_id != APPLICATION_ID or user_version != SCHEMA_VERSION:
        conn.close()
        raise DocStoreError(ErrorKind.SCHEMA, f"unsupported database (application_id={app_id:#x}, user_version={user_version}; expected {APPLICATION_ID:#x}/{SCHEMA_VERSION})", path)
    return conn


def _fetch_rows_and_revision(conn: sqlite3.Connection, path: Path) -> tuple[list[Row], int]:
    """Reads every row and meta.revision; must run inside a transaction."""
    meta = dict(conn.execute("SELECT key, value FROM meta").fetchall())
    if meta.get("schema_version") != str(SCHEMA_VERSION):
        raise DocStoreError(ErrorKind.SCHEMA, f"meta.schema_version={meta.get('schema_version')!r}, expected '{SCHEMA_VERSION}'", path)
    revision_text = meta.get("revision")
    if revision_text is None or not revision_text.isascii() or not revision_text.isdigit():
        raise _malformed(f"meta.revision={revision_text!r} is not an unsigned integer", path)
    rows = [Row(*record) for record in conn.execute("SELECT path, parent, seg, kind, payload FROM frag").fetchall()]
    return rows, int(revision_text)


def _read_db(path: Path) -> Any:
    conn = _connect(path, write=False)
    try:
        conn.execute("BEGIN")
        rows, _revision = _fetch_rows_and_revision(conn, path)
        conn.execute("COMMIT")
    except sqlite3.Error as exc:
        raise _sqlite_error(exc, path, "read") from exc
    finally:
        conn.close()
    return join(rows, path)


def read_revision(path: Path) -> int:
    """meta.revision of an existing `.db` file (the Rust `Signature::Revision`)."""
    conn = _connect(path, write=False)
    try:
        conn.execute("BEGIN")
        _rows, revision = _fetch_rows_and_revision(conn, path)
        conn.execute("COMMIT")
        return revision
    except sqlite3.Error as exc:
        raise _sqlite_error(exc, path, "read") from exc
    finally:
        conn.close()


def _apply_diff(conn: sqlite3.Connection, old_rows: list[Row], new_rows: list[Row]) -> int:
    """Writes the row-level diff (clarification 9); returns the number of rows touched.

    Existing paths are UPDATEd in place (rowid kept), new paths INSERTed, vanished paths
    DELETEd - the same statement strategy as the Rust `sqlite::apply_diff`.
    """
    old_by_path = {row.path: row for row in old_rows}
    new_paths = {row.path for row in new_rows}
    touched = 0
    for row in new_rows:
        old = old_by_path.get(row.path)
        if old is not None and (old.parent, old.seg, old.kind) == (row.parent, row.seg, row.kind) and _payload_equal(row.kind, old.payload, row.payload):
            continue
        if old is None:
            conn.execute("INSERT INTO frag(path, parent, seg, kind, payload) VALUES (?, ?, ?, ?, ?)", (row.path, row.parent, row.seg, row.kind, row.payload))
        else:
            # UPDATE, never INSERT OR REPLACE: REPLACE re-inserts the row under a new max
            # rowid and leaves freed pages behind; an UPDATE rewrites it in its own leaf.
            cursor = conn.execute("UPDATE frag SET parent = ?, seg = ?, kind = ?, payload = ? WHERE path = ?", (row.parent, row.seg, row.kind, row.payload, row.path))
            if cursor.rowcount != 1:
                # Read in this same IMMEDIATE transaction, so it must exist; never silent.
                raise sqlite3.OperationalError(f"update of row {row.path!r} changed {cursor.rowcount} rows inside the write transaction (expected 1)")
        touched += 1
    for path in old_by_path.keys() - new_paths:
        conn.execute("DELETE FROM frag WHERE path = ?", (path,))
        touched += 1
    return touched


def _update_db(path: Path, mutator: Callable[[Any], None]) -> None:
    conn = _connect(path, write=True)
    try:
        try:
            conn.execute("BEGIN IMMEDIATE")
            old_rows, revision = _fetch_rows_and_revision(conn, path)
            document = join(old_rows, path)
            _run_mutator(mutator, document, path)
            new_rows = split(document)
            touched = _apply_diff(conn, old_rows, new_rows)
            if touched:
                conn.execute("UPDATE meta SET value = ? WHERE key = 'revision'", (str(revision + 1),))
                conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('writer', ?)", (WRITER_NAME,))
            conn.execute("COMMIT")
        except BaseException:
            if conn.in_transaction:
                conn.execute("ROLLBACK")
            raise
    except sqlite3.Error as exc:
        raise _sqlite_error(exc, path, "update") from exc
    finally:
        conn.close()
    if touched:
        log.info("docstore: updated %s (%d row(s) changed, revision %d -> %d)", path, touched, revision, revision + 1)
    else:
        log.debug("docstore: %s unchanged, nothing written", path)


def _create_database(path: Path, value: Any, *, doc_kind: str, writer: str = WRITER_NAME, app_version: str = "") -> None:
    """Creates a NEW `.db` holding `value` (tests and fixture tooling only; runtime never creates).

    Raises DocStoreError(IO) when `path` already exists.
    """
    if path.exists():
        raise DocStoreError(ErrorKind.IO, "refusing to overwrite an existing file", path)
    rows = split(value)
    conn = sqlite3.connect(str(path), isolation_level=None)
    try:
        conn.execute(f"PRAGMA application_id={APPLICATION_ID}")
        conn.execute(f"PRAGMA user_version={SCHEMA_VERSION}")
        conn.execute("PRAGMA journal_mode=DELETE").fetchall()
        conn.execute("PRAGMA synchronous=FULL")
        conn.execute("BEGIN IMMEDIATE")
        for statement in _SCHEMA_SQL.split(";"):
            if statement.strip():
                conn.execute(statement)
        meta = {"schema_version": str(SCHEMA_VERSION), "doc_kind": doc_kind, "revision": "0", "writer": writer, "app_version": app_version}
        conn.executemany("INSERT INTO meta(key, value) VALUES (?, ?)", list(meta.items()))
        conn.executemany("INSERT INTO frag(path, parent, seg, kind, payload) VALUES (?, ?, ?, ?, ?)", [(r.path, r.parent, r.seg, r.kind, r.payload) for r in rows])
        conn.execute("COMMIT")
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# JSON codec
# ---------------------------------------------------------------------------


def _read_json(path: Path) -> Any:
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise DocStoreError(ErrorKind.NOT_FOUND, "document vanished while reading", path) from exc
    except (OSError, UnicodeDecodeError) as exc:
        raise DocStoreError(ErrorKind.IO if isinstance(exc, OSError) else ErrorKind.MALFORMED, f"cannot read: {exc}", path) from exc
    try:
        return parse_json(text)
    except ValueError as exc:
        raise _malformed(f"not valid JSON: {exc}", path) from exc


def _fsync_directory(directory: Path) -> None:
    """Makes a rename durable on POSIX; Windows has no directory handles to fsync."""
    if os.name == "nt":
        return
    fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _write_json_atomic(path: Path, value: Any) -> None:
    """Temp file `.{name}.{pid}.tmp` + fsync + os.replace (+ dir fsync); the old file survives any failure."""
    _validate_value(value)
    text = json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False)
    temp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        with temp.open("w", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp, path)
        _fsync_directory(path.parent)
    except OSError as exc:
        try:
            temp.unlink(missing_ok=True)
        except OSError as cleanup_exc:
            log.error("docstore: could not remove temp file %s: %s", temp, cleanup_exc)
        raise DocStoreError(ErrorKind.IO, f"atomic write failed: {exc}", path) from exc


def _update_json(path: Path, mutator: Callable[[Any], None]) -> None:
    document = _read_json(path)
    original = copy.deepcopy(document)
    _run_mutator(mutator, document, path)
    if values_equal(original, document):
        log.debug("docstore: %s unchanged, nothing written", path)
        return
    _write_json_atomic(path, document)
    log.info("docstore: updated %s", path)


# ---------------------------------------------------------------------------
# Public document API
# ---------------------------------------------------------------------------


_LOCKS_GUARD = threading.Lock()
_LOCKS: dict[str, threading.Lock] = {}


def _stem_lock(stem: Path) -> threading.Lock:
    """Process-local lock per stem: serializes this process's writers (cross-process: SQLite locks)."""
    key = os.fspath(stem.resolve())
    with _LOCKS_GUARD:
        return _LOCKS.setdefault(key, threading.Lock())


def _read_file(fmt: Format, path: Path) -> Any:
    if fmt is Format.DB:
        if not is_sqlite_file(path):
            raise _malformed("file is not an SQLite database (header sniff failed)", path)
        return _read_db(path)
    return _read_json(path)


def _run_mutator(mutator: Callable[[Any], None], document: Any, path: Path) -> None:
    if not isinstance(document, dict):
        raise _malformed(f"document root is {type(document).__name__}, expected an object", path)
    try:
        mutator(document)
    except Exception as exc:
        raise DocStoreError(ErrorKind.MUTATOR, f"mutator failed: {exc}", path) from exc


def read_document(stem: Path) -> Optional[Any]:
    """The whole document at `stem` as a JSON value, or None when it does not exist.

    Raises DocStoreError (MALFORMED / SCHEMA / SQLITE / AMBIGUOUS / IO); never writes.
    """
    stem = Path(stem)
    located = _locate(stem)
    if located is None:
        return None
    if located.has_value:
        return located.value
    return _read_file(located.fmt, located.path)


def update_document(stem: Path, mutator: Callable[[dict], None]) -> None:
    """Serialized read-modify-write of the EXISTING object document at `stem`.

    The mutator edits the freshly read document in place and must touch only its own keys.
    `.db`: all inside one BEGIN IMMEDIATE (row-level diff, revision bump on change);
    `.json`: re-read, mutate, atomic replace (skipped when nothing changed).
    Raises DocStoreError: NOT_FOUND (never creates), RECONCILE_PENDING (both formats exist),
    MUTATOR (nothing written), and the read errors of `read_document`.
    """
    stem = Path(stem)
    with _stem_lock(stem):
        located = _locate(stem)
        if located is None:
            raise DocStoreError(ErrorKind.NOT_FOUND, "document does not exist; the application creates it", stem)
        if located.both_exist:
            raise DocStoreError(ErrorKind.RECONCILE_PENDING, "both .json and .db exist; writes wait until the application reconciles them", stem)
        if located.fmt is Format.DB:
            _update_db(located.path, mutator)
        else:
            _update_json(located.path, mutator)


def set_path(document: dict, keys: tuple[str, ...], value: Any) -> None:
    """Sets `document[k0][k1]...[kn] = value`, replacing non-object intermediates with `{}`."""
    if not keys:
        raise ValueError("set_path needs at least one key")
    node = document
    for key in keys[:-1]:
        nested = node.get(key)
        if not isinstance(nested, dict):
            nested = {}
            node[key] = nested
        node = nested
    node[keys[-1]] = value
