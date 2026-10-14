# Module: crates/ms-platform

## Purpose
GUI-free platform seams. The one owner of "does this platform offer X" (`PlatformFeature`),
of file open/save dialogs (`file_dialog`), of system clipboard reads (`clipboard`), of images
other apps hand over (`incoming`: Android Share / "Open with"), of the Android credential store
(`credentials`) and of the pre-window native error box (`message`). The only crate that names `rfd` and `arboard`; its
Android backends talk to the JVM through `jni` + `ndk-context`. Level 2 of the crate stack
(deps: `ms-i18n`, `ms-log`); compiles for desktop, wasm32 and Android.

## Architecture
- **Capability table** (`features.rs`): `PlatformFeature::is_available()` is a `const fn` over
  `cfg!`, so code stays compiled on every target and a gated branch folds away. Gated entry
  points return the typed `Unavailable { feature, reason }` (`NotOnThisPlatform` /
  `BridgeMissing` / `PenTouchIndistinguishable`), whose `Display` is a localized two-sentence
  message. `unavailable_reason()` gives the table's reason per target (`CanvasTouchGestures` on
  Windows: pen and touch indistinguishable until winit handles `PT_PEN`; also off on wasm).
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

- **Incoming images** (`incoming/`): every function exists on every target; outside Android it
  answers "nothing incoming". Android: see "Incoming inbox" below.

- **Credentials** (`credentials/`, Android + host tests only): blocking `read_blocking` /
  `store_blocking` / `delete_blocking` of one secret by (service, account), typed
  `CredentialError` (`Unavailable` = bridge missing from the APK, `Unreadable`, `Backend`).
  Desktop consumers keep `keyring` themselves and the web has no store, so the module does not
  exist there (no fake answer). See "Credential store" below.

## Credential store (Android)
- Owner of the storage: the Kotlin `CredentialBridge` (file header there). One AES-256-GCM key
  in the AndroidKeyStore (alias `io.github.vasyanator.manhwastudio.credentials.v1`,
  non-exportable, no user-auth requirement, created by the first write), entries in the
  app-private SharedPreferences file `ms_credentials` as `1:<b64 IV>:<b64 ciphertext+tag>`,
  fresh Keystore IV per write, the entry name as GCM AAD. Alias, file name and value format are
  persistence contracts.
- Entry name (`entry_name`, pure, host-tested, frozen): `"v1." + hex(service) + "." +
  hex(account)`; Kotlin treats it as opaque.
- Unreadable is never "missing": an entry that exists but does not decrypt (Keystore key lost
  or invalidated, malformed or tampered value) is `CredentialError::Unreadable`; nothing deletes
  or rewrites it except an explicit store (replaces it) or delete. Consumers surface it as a
  read error.
- Wire contract (both sides change together): class name `CREDENTIAL_CLASS`
  (`credentials/android.rs`); `read(Context, String) -> String[]` = `{"ok", secret}` /
  `{"missing"}` / `{"unreadable", detail}` / `{"failed", detail}` (decoded by
  `decode_read_answer`, a malformed answer never echoes a field); `write` / `delete` answer
  `String?` (null = done). Kotlin never throws into native and serializes all operations.
- Secrets never reach a log, an error or a `Debug` output; logs carry the operation and the
  typed error only. Verified on a device only: the Keystore key, encryption, persistence across
  restarts and the unreadable path.

## Incoming inbox (Android)
- Owner of the layout: the Kotlin `IncomingImages` (file header there). `LaunchActivity`
  receives VIEW / SEND / SEND_MULTIPLE, copies every document (while it still holds the read
  grants) into `<cacheDir>/incoming/<millis>-<n>/NNNNN/<name>` and publishes the batch:
  `batch.tsv` (ordered `f\t<path>` / `s\t<name>\t<code>\t<detail>` lines) and then the
  `ready` marker. NOT `saf_in`: the SAF bridge purges that root on its first request.
- Hand-off is pull-plus-wake: Kotlin calls the native `nativeOnIncoming` (bound with
  `RegisterNatives`) only after `markNativeReady`; the native raises an `AtomicBool` and calls
  the registered `Waker`, nothing else. The GUI polls `has_pending()` (no JNI) and runs
  `take_pending_blocking()` on a worker, which clears the flag BEFORE asking Kotlin, so a batch
  published meanwhile is never missed. `takePending` renames `ready` to `claimed`.
- Cold start: `init()` once per native session on the `run_main` thread before the first
  frame (documented exception to "no JNI on the GUI thread"): load class, bind native,
  `markNativeReady`, `sweepClaimed` (deletes batches an earlier session claimed and unfinished
  leftovers), `takePending`.
- Decoding (`incoming/listing.rs`, pure): a malformed entry (field count, unknown code, path
  outside its batch folder) fails the WHOLE take with `IncomingError::Listing` and releases
  every batch of it; empty batches are released silently. The consumer releases a taken batch
  with `release_batch_blocking(batch_dir)` once its files are copied into a session.
- Wire contract (both sides change together): the class name `INCOMING_CLASS`
  (`incoming/android.rs`), the static method signatures listed in that file's header, the
  listing format and the skip codes `copy_failed` / `no_stream`.

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
- `features.rs`: the capability table (`is_available`, `unavailable_reason`) and `Unavailable`.
- `file_dialog/mod.rs`: request/result types, `PendingDialog`, single-flight, outcome logging,
  kind -> `Picked` mapping. `tests.rs` holds its host tests.
- `file_dialog/{desktop,android,unavailable}.rs`: one backend per target (dispatch via
  `use … as backend`).
- `file_dialog/mime.rs`: filter -> MIME mapping for SAF. One unmapped extension widens the whole
  request to `*/*` (logged at info with the extension); callers guard their filters with a test.
- `clipboard/{mod,desktop,android,unavailable}.rs`: clipboard API and backends.
- `incoming/mod.rs`: the cross-target API (`IncomingBatch`, `IncomingSkip`, `IncomingSkipKind`,
  `IncomingError`, `init`, `set_waker`, `has_pending`, `take_pending_blocking`,
  `release_batch_blocking`) and the pending flag + waker.
- `incoming/listing.rs`: pure decoding of the Kotlin listing and the release policy (host-tested).
- `incoming/android.rs`: the JNI half (class load, native binding, static calls).
- `message.rs`: `show_error_blocking` (startup only).
- `credentials/mod.rs`: `CredentialError`, the Android operations, the pure `entry_name` and
  `decode_read_answer`; `credentials/android.rs`: the JNI calls; `credentials/tests.rs`: host
  tests (frozen names, answer decoding, messages).
- `android/`: `jni_ctx.rs` (Android only; also the shared `read_string_array`), `saf.rs` (pure SAF half, host-tested in
  `saf/tests.rs`: the bridge class name `SAF_BRIDGE_CLASS`, wire codes and decoders, the
  pending-request registry, staging name rules).
- Kotlin half: `android/app/src/main/kotlin/io/github/vasyanator/manhwastudio/platform/`
  (`SafBridge.kt`, `SafRequestActivity.kt`, `IncomingImages.kt`, `CredentialBridge.kt`; manifest entry for
  `SafRequestActivity`). The "Open with" MIME list of the manifest is guarded against
  `ms_config::single_image::INPUT_FILE_TYPES` by `crates/ms-config/tests/android_manifest_mime.rs`.

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
- Credential operations are blocking (Keystore binder calls, synchronous storage commit):
  callers run them on workers only.
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
  `message_key`, and `unavailable_reason` when the reason is not `NotOnThisPlatform`) plus
  the `platform.unavailable.*_error` key in `en.json` and `ru.json`.
- New dialog kind: `DialogKind` (append only: its index is the Kotlin bridge contract),
  `picked_from_paths`, each backend, and the `Picked` table.
- New extension family for SAF: `file_dialog/mime.rs`.
- Android SAF launch / result / commit path: `file_dialog/android.rs` (JNI) +
  `android/saf.rs` (wire codes, names) + the Kotlin `SafBridge.kt` / `SafRequestActivity.kt`.
- Android incoming images: Kotlin `IncomingImages.kt` (layout, copy, markers) +
  `incoming/android.rs` (JNI) + `incoming/listing.rs` (wire decoding); the intent filters are in
  `android/app/src/main/AndroidManifest.xml` (`LaunchActivity`).
- Android clipboard reads: `clipboard/android.rs` + the `read_text_backend` dispatch in
  `clipboard/mod.rs`.
- Android credential store: Kotlin `CredentialBridge.kt` (storage, crypto) +
  `credentials/android.rs` (JNI) + `credentials/mod.rs` (names, answer decoding, errors); new
  message keys `platform.credentials.*_error` in `en.json` and `ru.json`.
