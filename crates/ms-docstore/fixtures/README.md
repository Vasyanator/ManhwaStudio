# ms-docstore golden fixtures

Shared contract between the two implementations of the document-store row codec:
the Rust crate `crates/ms-docstore` (SQLite `.db` codec) and the Python backend's
`docstore.py` at the repository root. Both MUST reproduce these files exactly; a
mismatch means one side drifted from the schema and must be fixed, not the fixture.

## Format

For every case `<case>`:

- `<case>.json` — the logical document (any JSON value).
- `<case>.rows.json` — the `frag` rows `split(<case>.json)` must produce, as a JSON
  array sorted by `path` (code-point order, identical to UTF-8 byte order), each row
  an object `{"path", "parent", "seg", "kind", "payload"}`; SQL NULL is JSON `null`.

Rows are compared field by field as strings (byte-for-byte). `join(rows)` must give a
value equal to `<case>.json` under type-strict JSON equality (`1` and `1.0` differ).

## The split rule in one screen

The binding text is section C of the storage design plus the "SPEC CLARIFICATIONS"
block at the top of `docstore.py`; in short:

- Root row: path `""`, parent NULL, seg NULL. A child's parent is the parent row's path
  (`""` for top-level children); path = parent path + `/` + RFC 6901-escaped seg
  (`~` -> `~0`, `/` -> `~1`); seg holds the RAW segment text.
- Object -> `obj` row, payload NULL, one child per key (seg = key).
- Array, id-keyed (non-empty, all objects, key `id` if the first element has it, else
  `uid`; every element's key is a JSON string or integer — not bool/float; key texts
  distinct) -> `arr_id` row (payload = compact JSON array of the keys in order) plus one
  `ent` row per element (seg = compact JSON text of the key, e.g. `"a1"` or `12`;
  payload = compact JSON of the whole element).
- Array with no object/array element (including `[]`) -> one `leaf` row (compact JSON).
- Any other array -> `arr` row (payload = element count as decimal text) with positional
  children (seg `"0"`, `"1"`, ...).
- Scalar -> `leaf` row with compact JSON (`null` -> `"null"`).
- Compact JSON = `serde_json::to_string` of a BTreeMap `Value` =
  `json.dumps(v, ensure_ascii=False, separators=(",", ":"), sort_keys=True)`.

## Floats

Integer lexemes agree in both languages. Float lexemes agree only for `0` and
`1e-4 <= |x| < 1e16`; outside that window Python and serde_json use different
exponent spellings. Fixtures therefore keep floats inside the window, and both diff
writers treat a payload as unchanged when its text differs but its parsed value is
type-strictly equal.

## Cross-language database check

`MS_DOCSTORE_FIXTURE_OUT=<dir>` makes the Rust tests write `<case>.db` for every case;
`MS_DOCSTORE_FIXTURE_IN=<dir> venv/bin/python -m pytest test_docstore.py` then opens each
Rust-written database with the Python codec and compares it to `<case>.json`.

Reverse direction: write every case with `docstore._create_database(<dir>/<case>.db, value,
doc_kind="user_config")` and run `MS_DOCSTORE_FIXTURE_IN=<dir> cargo test -p ms-docstore
python_written`; the Rust test compares the stored rows and the read-back value.

## Adding a case

Add `<case>.json`, generate `<case>.rows.json` with `docstore.split` (Python) or the Rust
`split`, hand-check every row against the rules above, and run both test suites.
