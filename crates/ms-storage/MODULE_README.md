# Module: crates/ms-storage

## Purpose
Storage abstraction that decouples ManhwaStudio's persistence from `std::fs`, so
the same application code runs on the desktop (real filesystem) and in the
browser (no filesystem). This is the foundation of the web (wasm) port's
"projects in site data" requirement.

## Architecture
A single synchronous, object-safe `Storage` trait plus two backends that behave
identically (guaranteed by shared contract tests in `tests/backends.rs`):

- **`NativeStorage`** — desktop backend. Resolves virtual paths under a fixed
  root `PathBuf` and delegates to `std::fs`. Never touches anything outside its
  root.
- **`MemStorage`** — in-memory virtual filesystem. On the web build this is the
  live session store: the app's web layer hydrates it from browser IndexedDB at
  startup and flushes it back at save checkpoints (that async bridge is NOT in
  this crate). Also the default backend for fast unit tests. Cloneable handles
  share one tree via `Arc<RwLock<_>>`, matching how the app shares one
  `Arc<dyn Storage>` across worker threads.

Paths are **virtual**: root-relative, '/'-separated (backslashes accepted),
`.`/`..` resolved lexically by `path::normalize` (a `..` above the root is a hard
`StorageError::Escape`, never a real traversal).

## Files and submodules
- `lib.rs`: `Storage` trait, `StorageError`, `Metadata`, `DirEntry`; re-exports.
- `native.rs`: `NativeStorage` + `real_path()` migration helper (virtual → real
  `PathBuf`, for call sites still handing a `&Path` to e.g. `image::open`).
- `mem.rs`: `MemStorage` in-memory tree backend.
- `path.rs`: `normalize()` virtual-path parser + unit tests.
- `tests/backends.rs`: identical-behavior contract tests across both backends.

## Contracts and invariants
- The trait is **object-safe** — hold it as `Arc<dyn Storage>`; pick the backend
  at startup.
- Semantics mirror `std::fs`: `write`/`rename` do NOT create missing parent
  dirs; call `create_dir_all` first (the desktop code already does).
- All methods are total (no panics): lock poisoning is recovered via
  `PoisonError::into_inner`, oversized lengths saturate instead of overflowing.
- Backends MUST stay behaviorally identical; any new method needs a matching
  case in `tests/backends.rs`.

## Editing map
- To add an operation (e.g. `copy`), add it to the `Storage` trait in `lib.rs`,
  implement in both `native.rs` and `mem.rs`, and add a contract test.
- To add a new backend (e.g. an OPFS sync-access-handle backend for web workers),
  add a `mod`, implement `Storage`, and include it in the `backends()` fixture.
- Path rules live only in `path.rs` — do not re-implement normalization in a
  backend.
