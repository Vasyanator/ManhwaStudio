# Module: crates/ms-browser-ext/src

## Purpose
The ManhwaStudio browser extension as part of the app: its pinned Chromium id, the extension
bundle embedded at build time, and the atomic, version-stamped unpack into the app's data
folder. Level 0 of the workspace (no `ms-*` dependencies). GUI-free, no i18n, no logging,
typed errors; callers (the bridge wiring, the translation tab) own threads and user text.

## Architecture
```
build.rs (host, at build time)
  manifest.json key ──derive::extension_id_from_public_key──> OUT_DIR/ids.rs
  [node + npm] lock node_modules.lock -> npm ci (lockfile hash stamp) -> node build.mjs
     -> OUT_DIR/ext-staging + build-info.json -> OUT_DIR/bundle.rs (include_bytes! list)
  [no node / skip / failure] -> bundle.rs = Err(NotIncluded{reason}) + cargo:warning
runtime
  bundle() -> &Bundle | NotIncluded
  install(bundle, install_dir(data_dir)) -> files (temp+fsync+rename), stale removal, stamp last
  installed_build_id(dir) -> stamp build_id
```

## Files and submodules
- `lib.rs`: crate root, re-exports, `CHROMIUM_EXTENSION_ID` (generated), `chromium_origin()`.
- `bundle.rs`: `Bundle`, `NotIncluded`, `bundle()`; includes the generated `bundle.rs`.
- `install.rs`: `install_dir`, `installed_build_id`, `install`, `InstallReport`,
  `InstallError`; tests in `install/tests.rs`.
- `names.rs`: reserved file names (`BUILD_INFO_FILE_NAME`, `STAMP_FILE_NAME`); shared with
  `build.rs` via `#[path]`.
- `derive.rs`: id rule and `build_id` rule; shared with `build.rs` via `#[path]`, compiled in
  the crate for tests only.
- `../build.rs`: the bundle pipeline above (header documents every step).

## Contracts and invariants
- `CHROMIUM_EXTENSION_ID` is a fixed contract (the bridge pins `Origin` to
  `chromium_origin()`): it changes only if `browser-extension/manifest.json` `key` changes,
  which must never happen. A manifest without `key` fails the build.
- `build_id` = first 16 bytes of sha256 over the staged files sorted by path, each fed as
  `path, 0x00, u64 LE length, bytes`, excluding `build-info.json` (which carries it). It is
  NOT a compatibility gate (decision D1: only the wire `PROTOCOL_VERSION` is); it decides
  re-unpack and extension reload.
- A bundle never contains `bridge.json`, `build-info.json` from the TS build, or the stamp.
- `install` is crash-safe: every file is temp + fsync + rename, the stamp is written last
  (after a directory fsync on unix), so "stamp `build_id` == bundle `build_id`" means complete.
  It deletes only files the previous stamp listed (and only valid relative paths), so
  `bridge.json` and user files survive. Blocking I/O: worker threads only.
- One install per folder at a time, across processes: `install` holds an exclusive advisory
  lock on `.ms-install.lock` in the folder from the old stamp's read through the final stamp
  write; a concurrent installer (another app version) waits, then works on the settled folder.
  The lock file is never a bundle path and is never deleted. A caller's in-process "busy"
  answer (`ms_browser_bridge::app::install_guarded`) is UI behaviour on top, not the
  filesystem serialization.
- The folder `<data_dir>/browser_extension/chromium` never moves: the browser keeps the
  "Load unpacked" registration by path.
- Node/npm are optional at build time; `MS_SKIP_BROWSER_EXTENSION=1` builds without the
  extension. A failed npm build never fails the cargo build (warning + `NotIncluded`).
- The build script watches only existing source paths (never `node_modules` or PATH) and is
  the only npm-running build script in the workspace; other crates do not wait on it unless
  they depend on this crate.

## Editing map
- Folder location or stamp format: `install.rs` (+ `names.rs` for the stamp name).
- Bundle contents, npm invocation, skip rules: `../build.rs`.
- Id or `build_id` rule: `derive.rs` (changing the id rule breaks the pinned origin).
- The TypeScript sources: `browser-extension/` (its own `MODULE_README.md`).
