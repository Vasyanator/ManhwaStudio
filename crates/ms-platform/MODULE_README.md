# Module: crates/ms-platform

## Purpose
GUI-free platform seams. The one owner of "does this platform offer X" (`PlatformFeature`),
of file open/save dialogs (`file_dialog`), of system clipboard reads (`clipboard`) and of the
pre-window native error box (`message`). The only crate that names `rfd` and `arboard`; its
Android backends talk to the JVM through `jni` + `ndk-context`. Level 2 of the crate stack
(deps: `ms-i18n`, `ms-log`); compiles for desktop, wasm32 and Android.

## Architecture
- **Capability table** (`features.rs`): `PlatformFeature::is_available()` is a `const fn` over
  `cfg!`, so code stays compiled on every target and a gated branch folds away. Gated entry
  points return the typed `Unavailable { feature, reason }` (`NotOnThisPlatform` /
  `BridgeMissing`), whose `Display` is a localized two-sentence message.
- **File dialogs** (`file_dialog/`): `request(req, waker) -> PendingDialog` never blocks; the
  GUI polls the handle each frame. `pick_blocking(req)` is the worker form. A backend gets a
  `Completion` and calls `finish(result)` exactly once; dropping it unfinished reads as
  `WorkerLost`. Backend per target:
  - desktop (`desktop.rs`): rfd on a detached `file-dialog` thread;
  - Android (`android.rs`): the Storage Access Framework bridge to the Kotlin `SafBridge`
    (see "SAF bridge" below); an APK without the class answers
    `Unavailable { FilePicker, BridgeMissing }`;
  - wasm (`unavailable.rs`): `Unavailable { FilePicker, NotOnThisPlatform }`.
- **Clipboard** (`clipboard/`): blocking, short reads. Desktop = arboard; Android text = JNI
  `ClipboardManager` on the calling thread (`android.rs`); Android image / file list / HTML
  and every wasm read = typed `Unavailable`.
- **JNI** (`android/jni_ctx.rs`): `with_env` attaches the thread and passes the application
  context; `load_app_class` uses `Context.getClassLoader()` because `FindClass` on a native
  thread cannot see app classes.

## SAF bridge (Android)
- Flow: `request` registers the `Completion` and spawns a `platform-bridge` worker. The worker
  loads `SafBridge` (application class loader, cached; `nativeOnResult` bound with
  `RegisterNatives`, so the binding never depends on the cdylib's symbol export), calls `SafBridge.launch` (returns at
  once; Kotlin starts the translucent `SafRequestActivity` on the UI thread over the last
  resumed Activity), then blocks on a channel until the native
  `Java_io_github_vasyanator_manhwastudio_platform_SafBridge_nativeOnResult` delivers the one
  result for its request id. Open kinds are then copied in by the worker (`SafBridge.copyToFile`
  / `listTree`); save kinds become staged targets.
- Wire contract (both sides change together): kind codes = `DialogKind` order 0..4; status
  0 picked / 1 cancelled / 2 failed; `nativeOnResult -> false` = Rust did not take over the URI
  grants, Kotlin releases them; blocking helpers answer `String?` (null = ok), `listTree` the
  encoding of `saf::decode_tree_listing`, `treeHasEntries` 1 / 0 / -1. Kotlin never throws into
  native and calls `nativeOnResult` exactly once per accepted launch (a start that never
  happens fails after 10 s).
- Local layout: inputs `<cacheDir>/saf_in/<request id>/`, outputs
  `<noBackupFilesDir>/saf_out/<request id>/`; names are sanitized and de-duplicated
  (`android/saf.rs`), a created document's staging name keeps the requested extension. Both
  roots are purged on the first request of a process; a committed target removes its staging
  dir; copied-in inputs stay until the next process (callers read them after the pick).
- Grants: the picker's transient grants belong to `SafRequestActivity`, which finishes right
  after reporting, so Kotlin persists them first; Rust releases them after copy-in / `commit`
  (or when a result is discarded); `SafBridge` releases leftovers of an earlier process.
- Cancel: dropping the `PendingDialog` withdraws it; a running copy-in stops before the next
  file and deletes its copies. The picker itself stays until the user closes it.

## Files and submodules
- `lib.rs`: crate root, re-exports `PlatformFeature`, `Unavailable`, `UnavailableReason`.
- `features.rs`: the capability table and `Unavailable`.
- `file_dialog/mod.rs`: request/result types, `PendingDialog`, single-flight, outcome logging,
  kind -> `Picked` mapping. `tests.rs` holds its host tests.
- `file_dialog/{desktop,android,unavailable}.rs`: one backend per target (dispatch via
  `use … as backend`).
- `file_dialog/mime.rs`: pure filter -> MIME mapping for SAF.
- `clipboard/{mod,desktop,android,unavailable}.rs`: clipboard API and backends.
- `message.rs`: `show_error_blocking` (startup only).
- `android/`: `jni_ctx.rs` (Android only), `saf.rs` (pure SAF half, host-tested in
  `saf/tests.rs`: the bridge class name `SAF_BRIDGE_CLASS`, wire codes and decoders, the
  pending-request registry, staging name rules).
- Kotlin half: `android/app/src/main/kotlin/io/github/vasyanator/manhwastudio/platform/`
  (`SafBridge.kt`, `SafRequestActivity.kt`; manifest entry for the latter).

## Contracts and invariants
- One file dialog in flight process-wide: a second request resolves to
  `FileDialogError::Busy`. The flag is released before the result is delivered, so the
  requester may open the next dialog at once.
- `PendingDialog::poll` returns `Some` exactly once; `Ok(None)` is a cancel. The `Picked`
  variant always matches the `DialogKind` (table on `Picked`).
- Write the COMPLETE output to `SaveTarget::write_path` / `FolderTarget::write_dir`, then call
  `commit()` on the worker that wrote it (desktop: no-op; Android: blocking stream to the URI).
- Desktop `SaveFile`: a name typed WITHOUT any extension gets the first extension of the first
  filter (`with_default_extension`); a path that already has an extension is never changed.
  Callers therefore never re-derive the extension themselves.
- Filter extensions are normalized to lowercase without the dot; the desktop backend adds the
  uppercase spelling because GTK / xdg-portal globs are case-sensitive.
- `PickDirectoryPath` needs `PersistentDirectoryPicker` (unavailable on Android: SAF yields
  tree URIs, never usable paths).
- No JNI on the GUI thread: Android backends run every JVM call on a worker
  (`FolderTarget::has_entries` answers from a probe taken at pick time). The one
  exception is the synchronous clipboard text read (`clipboard/android.rs`): a single short
  binder round trip, the same cost class as the desktop arboard read it replaces.
- Logs: outcome per dialog at info level with the kind and file names only (never full paths);
  failures at warn level with the typed error.
- No egui types: callers own the UI and pass a repaint `Waker`.
- Test-only doors (`PendingDialog::ready`, `SaveTarget::for_test`, `FolderTarget::for_test`)
  sit behind `cfg(any(test, feature = "test-support"))`; enable `test-support` only from a
  dependent's `[dev-dependencies]`.

## Editing map
- New capability or a platform answer: `features.rs` (`PlatformFeature`, `ALL`,
  `message_key`) plus the `platform.unavailable.*_error` key in `en.json` and `ru.json`.
- New dialog kind: `DialogKind` (append only: its index is the Kotlin bridge contract),
  `picked_from_paths`, each backend, and the `Picked` table.
- New extension family for SAF: `file_dialog/mime.rs`.
- Android SAF launch / result / commit path: `file_dialog/android.rs` (JNI) +
  `android/saf.rs` (wire codes, names) + the Kotlin `SafBridge.kt` / `SafRequestActivity.kt`.
- Android clipboard reads: `clipboard/android.rs` + the `read_text_backend` dispatch in
  `clipboard/mod.rs`.
