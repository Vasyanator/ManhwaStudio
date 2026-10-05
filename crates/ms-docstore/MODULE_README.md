# Module: crates/ms-docstore

## Purpose
The single owner of "read / update / write a named logical document": `user_config`,
`fonts_data`, `presets`, a title's `settings`/`characters`/`terms`/favorites/color presets,
and a chapter's `layers`/`bubbles`. Callers name a document with a `DocRef` and never open
its file themselves. A document lives as `<stem>.json` (Dev) or `<stem>.db` (Prod, `SQLite`
fragment store) behind one API; the Python backend mirrors the `.db` codec in the
repo-root `docstore.py` (it touches only `user_config`).

## Architecture
- Layer: depends on `ms-storage` (wasm seam + error vocabulary), `ms-log`, and natively
  `rusqlite` (`bundled`). It receives paths and must NEVER depend on `ms-config`.
- Every operation resolves the document's format (`resolve.rs`) and dispatches per format
  (`codec.rs`). Reads are unlocked; mutations run under a process-local per-stem lock;
  `with_lock` resolves the format once per critical section (`LockedDoc`).
- JSON writes: sibling temp `.{name}.{pid}.tmp` → fsync (unless `Durability::None`) →
  rename (Windows sharing-violation retry) → optional directory fsync. When the file
  already holds the exact bytes nothing is rewritten (no temp/rename): only the requested
  durability is applied to the existing file (`Contents` fsyncs it, `ContentsAndDirectory`
  also the directory), so an unchanged flush costs a stat + a cached read.
  `Durability::None` performs no fsync anywhere (neither temp nor directory).
- `write_bytes_atomic(path, bytes, durability)` (native) exposes that same recipe for files
  that are NOT owned documents (single-image Save overwriting the user's image). Parent must
  exist (never created); the old file is never deleted first; on failure it is intact and
  the temp is removed; the rename replaces the directory entry (a symlink is replaced, not
  followed — callers canonicalize); identical bytes are not rewritten; the caller logs.
- `.db` writes to an existing file: ONE `BEGIN IMMEDIATE` transaction reads every row,
  joins, applies the caller's change (mutator / baseline check), splits and writes only the
  row diff: changed rows `UPDATE ... WHERE path` in place (rowid kept — `INSERT OR REPLACE`
  would re-insert under a new rowid and churn free pages), new rows `INSERT`, vanished rows
  `DELETE`. An empty diff commits an empty transaction (no journal, no fsync, file mtime
  unchanged). The baseline fingerprint is computed only for a non-`Unchecked` baseline. New `.db` files and `write_whole_atomic` go through a validated temp + rename
  (`whole.rs`; the temp is fsynced through a WRITE handle — Windows `FlushFileBuffers`
  refuses a read-only one). Connections are short-lived (one per call); pragmas on every open:
  `busy_timeout=5000`, `journal_mode=DELETE` (explicit; resets a foreign WAL header),
  `synchronous=FULL`, `foreign_keys=OFF`.

## Files and submodules
- `src/lib.rs`: public types (`DocRef`, `DocKind`, `DocFormat`, `WriteOptions`, `Signature`,
  `Snapshot`, `LockedDoc`, `DocStoreError`, quarantine types) and every entry point.
- `src/codec.rs`: per-format read / write / update / remove / signature dispatch.
- `src/resolve.rs`: default format, resolution rules B.2-B.4, `chapter_new_format`.
- `src/split.rs`: pure `Value` ↔ `frag` rows (mirrors `docstore.py` split/join). Native.
- `src/sqlite.rs`: schema, open/pragmas, strict read, diff-write transaction, create. Native.
- `src/whole.rs`: whole-document temp → validate → commit (rename). Native.
- `src/convert.rs`: `convert_document` protocol, `convert_many`, `chapter_format_report`.
- `src/json.rs`: atomic write recipe, `Fingerprint`/`SaveBaseline`/`Durability`,
  `is_temp_artifact`, test step journal + fault injector (`test-support` feature).
- `src/fsio.rs`: exists/read/metadata/remove/rename/copy (`std::fs` native, seam on wasm).
- `src/lock.rs`: per-stem lock registry (normalized keys, poison recovery, pruning).
- `src/tests.rs`, `src/tests_db.rs`: contract tests; `fixtures/`: golden cases shared with
  Python (see `fixtures/README.md`).

## `.db` schema and split rule (binding; shared with `docstore.py`)
- `PRAGMA application_id=0x4D534453`, `user_version=1`; `meta(key,value)` with
  `schema_version='1'`, `doc_kind` (`DocKind::as_str`), `revision` (u64 text), `writer`
  (`rust`|`python`), `app_version` (informational, `''`); `frag(path PK, parent, seg, kind,
  payload)` + `INDEX frag_parent(parent)`.
- Root row: path `""`, parent/seg NULL. Child: parent = parent's path (`""` at top level),
  path = parent path + `/` + RFC 6901 esc(seg); seg is RAW text.
- Object → `obj` (payload NULL). Array id-keyed (non-empty, all objects, key `id` if the
  first element has it else `uid`, every element's key a string or integer — not bool or
  float —, compact key texts distinct) → `arr_id` (payload = compact key array) + one `ent`
  per element (seg = compact key text, e.g. `"a1"` with quotes; payload = whole element).
  Array with no object/array element (incl. `[]`) → one `leaf`. Other arrays → `arr`
  (payload = count) with children `"0".."n-1"`. Scalars → `leaf`.
- Compact JSON = `serde_json::to_string` (BTreeMap keys: `preserve_order` must stay off;
  `float_roundtrip` is on so text → f64 is exact).
- Join is strict (one root, exact `arr`/`arr_id` children, no duplicate seg, no orphan,
  known kinds, depth ≤ 512) → otherwise `Malformed`.
- Diff write: a row is unchanged when (parent, seg, kind) match and the payload is
  text-equal OR (for `leaf`/`ent`/`arr_id`) parses to a type-strictly equal `Value` (float
  rule: Python spells `1e-05` where serde writes `0.00001`). Only a non-empty diff bumps
  `revision` and sets `writer`, in the same transaction.

## Resolution and conversion
- B.2: exactly one file exists → that format, regardless of the default (a lone `.db`
  failing the 16-byte header sniff is `Malformed`, never "absent").
- B.3: neither exists → `DocRef::with_new_format` hint (chapter callers pass
  `chapter_new_format(siblings)`), else `default_format()`.
- B.4: both exist → the `default_format()` file is authoritative; readers read it; the
  first LOCKED operation validates it parses and deletes a Value-equal leftover, else
  `Ambiguous` (nothing is ever deleted then).
- `convert_document` holds the lock through: read source → temp in target format →
  reopen + Value-equal validation → fsync + rename + dir fsync → delete source. A crash at
  any step converges on the next run (or on the next locked access via B.4) provided the
  default format was switched to the target FIRST — drivers above this crate own that
  order, title/global enumeration and the mode key.

## Contracts and invariants
- The document lock is NON-reentrant; inside `with_lock` use `LockedDoc` methods only.
- `update` never overwrites a malformed document (`Malformed`, file untouched, mutator not
  run); a failing mutator writes nothing. Only `quarantine` moves a malformed file aside
  (sidecar name, `.db` journal included).
- `WriteOptions::default()` has `create_parent_dirs: true`: a write RE-CREATES a missing
  parent directory. A caller that must not resurrect a deleted directory (a staging dir being
  discarded) must stop its writers first or pass `false`. JSON `update` always rewrites; `.db` `update`
  writes nothing for an unchanged document.
- Baselines: JSON fingerprints the exact bytes; `.db`
  fingerprints the compact text of the joined `Value`. A baseline from one format never
  matches the other (one `Conflict` after a conversion → the caller's merge path).
- `.db` ignores `pretty`/`trailing_newline`/`Durability` BY DESIGN (always
  `synchronous=FULL`): a weaker mode can corrupt the database on power loss, which the
  "never torn" promise of every durability level forbids. The per-commit cost (~3 fsyncs,
  ~100-170 ms on an HDD) is therefore paid per transaction — callers batch writes.
- Replacing, removing or quarantining a `.db` handles its `-journal` (removed, or moved to
  `<sidecar>-journal`): SQLite would replay a stale journal into a new file of that name.
- Public error enum and `Signature` are matched exhaustively by callers — keep them stable: `SQLite` engine failures map to `Storage(StorageError::Io)` (busy →
  `ResourceBusy`), schema/foreign/corrupt files to `Malformed`, a newer schema to
  `Unsupported`, a failed whole-write validation to `Write(TempWrite)`. `Signature` is
  `Bytes` for both formats; `revision()` is the exact `.db` counter.
- wasm32: `DocFormat::Db` → `Unsupported`; `.db` files are never probed.
- Errors: technical `Display`; callers produce localized text. I/O-class write failures
  are logged here (not `DirSync`, which the caller logs).

## Known debt
Three private copies of the atomic-write recipe exist outside this crate and do not use
`write_bytes_atomic`: `ms-page-ops/src/fs_exec.rs::atomic_write` (also creates the parent and
deletes a stale destination before retrying the rename), `ms-tab-cleaning/src/tools/
watermark_library.rs::write_atomic_bytes` (creates the parent, no Windows retry), `ms-config/src/locale_store.rs::write_atomic`
(no fsync, no Windows retry; ms-config is above ms-docstore, so it could call it). Fold them
into `write_bytes_atomic` when their owners are next touched, after checking each one's
differing semantics.

## Editing map
- Schema / split rule: `split.rs` + `sqlite.rs` + `docstore.py` + `fixtures/` TOGETHER.
- Resolution rules: `resolve.rs`. Conversion protocol: `convert.rs` + `whole.rs`.
- JSON write recipe / durability: `json.rs`. Lock semantics: `lock.rs`.
- New document kind: `DocKind` variant + `DocKind::as_str` (frozen spelling).
