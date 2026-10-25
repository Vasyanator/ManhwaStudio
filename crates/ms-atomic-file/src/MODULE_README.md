# Module: crates/ms-atomic-file/src

## Purpose
The single owner of "replace a file's contents atomically" (sibling temp + fsync + rename) and
of the temp-name pattern that recipe produces. Level-0 foundation crate: std + `thiserror`, no
`ms-*` dependency, no logging; every crate that writes whole files in place uses it instead of a
private temp/rename copy. `ms-docstore` re-exports it unchanged (its documents' JSON write path
and whole-document commits run on it).

## Architecture
- `write_atomic(path, bytes, Durability)` is the default recipe; `write_atomic_with(path,
  bytes, &AtomicWriteOptions)` adds the per-site options: `create_parent`, `unix_mode`,
  `skip_if_identical` (on by default), `unique_temp`.
- Recipe: [create parent] -> [identical-bytes skip: apply durability to the existing file] ->
  temp (`.{name}.{pid}.tmp`, or `.{name}.{pid}.{serial}.tmp` with `create_new`) -> `write_all`
  -> `sync_all` (not with `Durability::None`) -> CLOSE -> `rename` (Windows: retried 10 x 20 ms
  on OS errors 5 / 32 / 33) -> [directory fsync with `ContentsAndDirectory`; no-op off Unix].
- The steps are public (`write_temp_file`, `rename_with_retry`, `sync_directory`) for
  multi-step commits that validate a staged temp before renaming it, and for publishes of an
  already-staged file that only need the retrying rename and the directory fsync.
- Every target compiles the same code: on wasm32 std's file system answers `Unsupported`, so a
  write fails with `AtomicWriteError::TempWrite { kind: Unsupported }`; callers keep their own
  wasm storage seams and need no `cfg` for this crate.

## Files and submodules
- `lib.rs`: crate root and re-exports.
- `recipe.rs`: `Durability`, `AtomicWriteOptions`, the recipe and its public steps, the
  Windows retry classifier.
- `naming.rs`: `temp_path_for`, the unique temp name, `is_temp_artifact`.
- `error.rs`: `AtomicWriteError` (`kind()`, `From<AtomicWriteError> for io::Error`).
- `test_hooks.rs`: `WriteStep` journal and `FaultPoint` injector (`cfg(test)` or the
  `test-support` feature; compiled to nothing otherwise).
- `tests.rs`: contract tests.

## Contracts and invariants
- The old file is never deleted before the rename. On every error it is intact, except
  `DirSync` (new bytes in place, directory entry not known-durable: do not delete the data's old
  home). The temp is removed on every failure of `write_atomic*` (cleanup failure is not
  reported: the write failure is the error). `write_temp_file` leaves cleanup to its caller.
- The temp handle is closed before any rename or removal (Windows refuses both on an open file).
- The rename replaces the directory entry: a symlink at `path` is replaced, not followed.
- Without `unique_temp`, writers of one path inside one process must be serialized by the
  caller (the temp name is per process).
- `unix_mode` is set at creation AND forced on the handle (a leftover temp keeps its old mode
  otherwise); the identical skip keeps an existing file only when its mode also matches.
- `is_temp_artifact` must keep accepting `.{name}.{pid}.tmp` (written by the Python mirror,
  repo-root `docstore.py`), the unique `.{name}.{pid}.{serial}.tmp` and a `.db` temp's
  `-journal`; a document's own `<name>.db-journal` is not a temp.
- Blocking I/O: never on the GUI thread unless `Durability::None`. Nothing is logged here.
- `AtomicWriteError`'s variants and fields are destructured by dependents (through
  `ms_docstore`): adding a field or variant breaks them.

## Editing map
- Change the recipe or add an option: `recipe.rs` (+ a test in `tests.rs`).
- Change temp naming: `naming.rs` — and the Python mirror `docstore.py` if the default name
  changes.
- Test observability for dependents: `test_hooks.rs`; `ms-docstore`'s `test-support` forwards it.
