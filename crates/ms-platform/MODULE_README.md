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
  - Android (`android.rs`): the SAF bridge via the pending registry (`android/saf.rs`); until
    the Kotlin `SafBridge` and its Rust launch path exist, a `platform-bridge` worker probes
    for the class and every request resolves to `Unavailable { FilePicker, BridgeMissing }`;
  - wasm (`unavailable.rs`): `Unavailable { FilePicker, NotOnThisPlatform }`.
- **Clipboard** (`clipboard/`): blocking, short reads. Desktop = arboard; Android and wasm =
  typed `Unavailable` (Android text answers `BridgeMissing` until its JNI backend lands).
- **JNI** (`android/jni_ctx.rs`): `with_env` attaches the thread and passes the application
  context; `load_app_class` uses `Context.getClassLoader()` because `FindClass` on a native
  thread cannot see app classes.

## Files and submodules
- `lib.rs`: crate root, re-exports `PlatformFeature`, `Unavailable`, `UnavailableReason`.
- `features.rs`: the capability table and `Unavailable`.
- `file_dialog/mod.rs`: request/result types, `PendingDialog`, single-flight, outcome logging,
  kind -> `Picked` mapping. `tests.rs` holds its host tests.
- `file_dialog/{desktop,android,unavailable}.rs`: one backend per target (dispatch via
  `use … as backend`).
- `file_dialog/mime.rs`: pure filter -> MIME mapping for SAF.
- `clipboard/{mod,desktop,unavailable}.rs`: clipboard API and backends.
- `message.rs`: `show_error_blocking` (startup only).
- `android/`: `jni_ctx.rs` (Android only), `saf.rs` (pending-request registry, also
  host-tested; the bridge class name `SAF_BRIDGE_CLASS`).

## Contracts and invariants
- One file dialog in flight process-wide: a second request resolves to
  `FileDialogError::Busy`. The flag is released before the result is delivered, so the
  requester may open the next dialog at once.
- `PendingDialog::poll` returns `Some` exactly once; `Ok(None)` is a cancel. The `Picked`
  variant always matches the `DialogKind` (table on `Picked`).
- Write the COMPLETE output to `SaveTarget::write_path` / `FolderTarget::write_dir`, then call
  `commit()` on the worker that wrote it (desktop: no-op; Android: blocking stream to the URI).
- Filter extensions are normalized to lowercase without the dot; the desktop backend adds the
  uppercase spelling because GTK / xdg-portal globs are case-sensitive.
- `PickDirectoryPath` needs `PersistentDirectoryPicker` (unavailable on Android: SAF yields
  tree URIs, never usable paths).
- No JNI on the GUI thread: Android backends run every JVM call on a worker.
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
- Android SAF launch / result path: `file_dialog/android.rs` + `android/saf.rs`.
- Android clipboard text: replace the `read_text` dispatch of `clipboard/mod.rs` on Android.
