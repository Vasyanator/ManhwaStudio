/*
File: file_dialog/android.rs

Purpose:
Android file-dialog backend over the Storage Access Framework: the JVM half of the bridge to
the Kotlin `SafBridge` (`android/app/src/main/kotlin/.../platform/SafBridge.kt`).

Main responsibilities:
- launch a request from a `platform-bridge` worker (`SafBridge.launch`, returns at once);
- receive the result (`nativeOnResult`: exported under its exact JNI name and bound with
  `RegisterNatives` when the class is loaded);
- copy picked documents / trees into app storage (cache dir, `saf_in/<request id>/`);
- stage save targets (no-backup files dir, `saf_out/<request id>/`) and publish them on
  `commit` (`SafBridge.streamFileToUri` / `copyDirIntoTree`);
- release the persisted URI grants once a document is copied in or committed.

Key structures:
- FileDocument / TreeDocument: the SAF half of `SaveTarget` / `FolderTarget`.
- TransferDirs: the inbox / outbox roots, purged once per process before the first request.

Key functions:
- launch(): the backend entry point (`super::request`).
- commit_file(), commit_folder(): `SaveTarget::commit` / `FolderTarget::commit` on Android.
- Java_io_github_vasyanator_manhwastudio_platform_SafBridge_nativeOnResult(): the result native.

Notes:
JNI contract (all static on `SafBridge`; Kotlin never throws into native, it answers with an
error value):
- `launch(Context, long requestId, int kind, String[] mimeTypes, String suggestedName): String`
  null = the picker will be shown and `nativeOnResult` called EXACTLY once for `requestId`;
- `nativeOnResult(long requestId, int status, String[] uris, String[] names, String error):
  boolean` (native; status codes in `android/saf.rs`); `false` tells Kotlin that Rust did not
  take over the URI grants of the report, so Kotlin releases them itself;
- blocking helpers, called only from Rust worker threads: `copyToFile(Context, uri, path)`,
  `streamFileToUri(Context, path, uri)`, `copyDirIntoTree(Context, dir, treeUri)` -> String
  (null = ok, else the error); `listTree(Context, treeUri)` -> String[] (`decode_tree_listing`);
  `treeHasEntries(Context, treeUri)` -> int (1 / 0 / -1 = error); `releaseUri(Context, uri)`.
Every JVM call runs on the `platform-bridge` worker or on the caller's commit worker, never on
the GUI thread; `FolderTarget::has_entries` answers from the probe made at pick time. Cancel:
dropping the `PendingDialog` withdraws the registration; a running copy-in notices it before
the next file, deletes what it copied and releases the grants. `PickDirectoryPath` never
reaches this backend (`PersistentDirectoryPicker` is unavailable on Android).
*/

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;

use jni::errors::Error as JniError;
use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::refs::Global;
use jni::sys::{jboolean, jint, jlong};
use jni::{Env, EnvUnowned, JValue, NativeMethod, Outcome, jni_sig, jni_str};
use ms_log::runtime_log;

use super::{Completion, DialogKind, DialogResult, DropGuard, FileDialogError, FileDialogRequest, FolderTarget, Picked, PickedFile, SaveTarget, mime_types_for};
use crate::android::jni_ctx::{self, JniBridgeError};
use crate::android::saf::{
    BridgeOutcome, PendingRegistry, PickedDocument, SAF_BRIDGE_CLASS, create_document_mime, decode_outcome, decode_tree_listing, extension_of, kind_code,
    name_with_default_extension, plan_file_copies, plan_tree_copy, sanitize_file_name, staging_file_name,
};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

/// Requests whose `PendingDialog` is still alive, keyed by an internal id.
static PENDING: PendingRegistry<Completion> = PendingRegistry::new();
/// Workers waiting for `nativeOnResult`, keyed by the request id passed to Kotlin.
static WAITERS: PendingRegistry<mpsc::Sender<BridgeOutcome>> = PendingRegistry::new();
/// The loaded `SafBridge` class (only a successful load is cached).
static BRIDGE: OnceLock<Global<JClass<'static>>> = OnceLock::new();
/// The transfer roots, resolved (and purged) on the first request of the process.
static TRANSFER_DIRS: OnceLock<TransferDirs> = OnceLock::new();

/// Directory under the cache dir that receives copied-in documents.
const INBOX_DIR_NAME: &str = "saf_in";
/// Directory under the no-backup files dir that holds staged outputs until `commit`.
const OUTBOX_DIR_NAME: &str = "saf_out";

/// The SAF half of a [`SaveTarget`]: the created document and the staging dir of its file.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FileDocument {
    uri: String,
    staging_dir: PathBuf,
}

/// The SAF half of a [`FolderTarget`]: the picked tree, the staging dir and the pick-time
/// "already has entries" probe (`Err` = the tree could not be listed).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TreeDocument {
    uri: String,
    staging_dir: PathBuf,
    entries: Result<bool, String>,
}

impl TreeDocument {
    /// The pick-time probe; no JVM call.
    ///
    /// # Errors
    /// [`FileDialogError::TargetIo`] when the tree could not be listed at pick time.
    pub(super) fn has_entries(&self) -> Result<bool, FileDialogError> {
        self.entries.clone().map_err(|detail| FileDialogError::TargetIo { detail })
    }
}

/// Local roots of the transfers (see the file header).
#[derive(Debug)]
struct TransferDirs {
    inbox: PathBuf,
    outbox: PathBuf,
}

/// What one request needs while it materializes its result.
struct Transfer<'a> {
    pending_id: u64,
    request_id: u64,
    bridge: &'a Global<JClass<'static>>,
    dirs: &'a TransferDirs,
}

impl Transfer<'_> {
    /// Whether the requester still waits (its `PendingDialog` was not dropped).
    fn still_wanted(&self) -> bool {
        PENDING.contains(self.pending_id)
    }
}

/// Withdraws a pending request when its `PendingDialog` is dropped; dropping the completion
/// releases the in-flight flag.
struct Registration(u64);

impl Drop for Registration {
    fn drop(&mut self) {
        drop(PENDING.take(self.0));
    }
}

/// Starts `request` on a `platform-bridge` worker.
///
/// # Errors
/// [`FileDialogError::Backend`] when the worker cannot be spawned.
pub(super) fn launch(request: FileDialogRequest, completion: Completion) -> Result<Option<DropGuard>, FileDialogError> {
    let pending_id = PENDING.register(completion);
    let spawned = std::thread::Builder::new().name("platform-bridge".to_owned()).spawn(move || {
        let Some(result) = run_request(pending_id, &request) else {
            return;
        };
        if let Some(completion) = PENDING.take(pending_id) {
            completion.finish(result);
        } else {
            runtime_log::log_info(format!("[file-dialog] {:?} request was withdrawn; late result discarded", request.kind()));
            discard_unclaimed(result);
        }
    });
    match spawned {
        // Detached on purpose: the result travels through the registry, never `join`.
        Ok(_detached) => Ok(Some(Box::new(Registration(pending_id)))),
        Err(err) => {
            drop(PENDING.take(pending_id));
            Err(FileDialogError::Backend { detail: err.to_string() })
        }
    }
}

/// The whole request on the worker: launch, wait, materialize. `None` = the requester went
/// away during the copy-in and everything was already cleaned up.
fn run_request(pending_id: u64, request: &FileDialogRequest) -> Option<DialogResult> {
    let kind = request.kind();
    let Some(code) = kind_code(kind) else {
        return Some(Err(Unavailable::new(PlatformFeature::PersistentDirectoryPicker, UnavailableReason::NotOnThisPlatform).into()));
    };
    let bridge = match bridge_class() {
        Ok(bridge) => bridge,
        Err(JniBridgeError::BridgeMissing { .. }) => {
            return Some(Err(Unavailable::new(PlatformFeature::FilePicker, UnavailableReason::BridgeMissing).into()));
        }
        Err(err) => return Some(Err(FileDialogError::Backend { detail: err.to_string() })),
    };
    let dirs = match transfer_dirs() {
        Ok(dirs) => dirs,
        Err(err) => return Some(Err(FileDialogError::Backend { detail: err.to_string() })),
    };
    let args = LaunchArgs::of(request);
    let (sender, receiver) = mpsc::channel();
    let request_id = WAITERS.register(sender);
    if let Err(err) = call_launch(bridge, request_id, code, &args) {
        drop(WAITERS.take(request_id));
        return Some(Err(FileDialogError::Backend { detail: err }));
    }
    // An accepted launch is answered exactly once (Kotlin contract), so this wait ends when
    // the user closes the picker; only the delivery removes the sender, after sending.
    let outcome = receiver.recv().unwrap_or_else(|_| BridgeOutcome::Failed("the picker result channel closed without a result".to_owned()));
    let documents = match outcome {
        BridgeOutcome::Picked(documents) => documents,
        BridgeOutcome::Cancelled => return Some(Ok(None)),
        BridgeOutcome::Failed(detail) => return Some(Err(FileDialogError::Backend { detail })),
    };
    let transfer = Transfer { pending_id, request_id, bridge, dirs };
    if kind == DialogKind::OpenFiles {
        return copy_in_files(&transfer, &documents);
    }
    // Every other kind picks exactly one document; `decode_outcome` never yields an empty pick.
    let Some(document) = documents.into_iter().next() else {
        return Some(Err(FileDialogError::Backend { detail: "the picker reported a choice without a document".to_owned() }));
    };
    match kind {
        DialogKind::OpenFile | DialogKind::OpenFiles => copy_in_files(&transfer, std::slice::from_ref(&document)),
        DialogKind::OpenFolder => copy_in_tree(&transfer, &document),
        DialogKind::SaveFile => Some(stage_file(&transfer, document, args.fallback_extension.as_deref())),
        DialogKind::SaveIntoFolder => Some(stage_folder(&transfer, document)),
        DialogKind::PickDirectoryPath => Some(Err(Unavailable::new(PlatformFeature::PersistentDirectoryPicker, UnavailableReason::NotOnThisPlatform).into())),
    }
}

/// The per-kind arguments of `SafBridge.launch`.
struct LaunchArgs {
    /// MIME filter of the open kinds; the created document's type for `SaveFile`.
    mime_types: Vec<&'static str>,
    /// Suggested name of `SaveFile` (with the default extension added).
    suggested_name: Option<String>,
    /// Extension the staging file falls back to when the created name has none.
    fallback_extension: Option<String>,
}

impl LaunchArgs {
    fn of(request: &FileDialogRequest) -> Self {
        let default_extension = request.filters.first().and_then(|filter| filter.extensions.first()).map(String::as_str);
        match request.kind {
            DialogKind::OpenFile | DialogKind::OpenFiles => {
                Self { mime_types: mime_types_for(&request.filters), suggested_name: None, fallback_extension: None }
            }
            DialogKind::SaveFile => {
                let name = name_with_default_extension(request.suggested_name.as_deref().unwrap_or_default(), default_extension);
                let extension = extension_of(&name).or(default_extension).map(str::to_owned);
                Self { mime_types: vec![create_document_mime(extension.as_deref())], suggested_name: Some(name), fallback_extension: extension }
            }
            DialogKind::OpenFolder | DialogKind::SaveIntoFolder | DialogKind::PickDirectoryPath => {
                Self { mime_types: Vec::new(), suggested_name: None, fallback_extension: None }
            }
        }
    }
}

/// Copies picked documents into `saf_in/<request id>/` (`OpenFile` / `OpenFiles`).
fn copy_in_files(transfer: &Transfer<'_>, documents: &[PickedDocument]) -> Option<DialogResult> {
    let inbox = transfer.dirs.inbox.join(transfer.request_id.to_string());
    let uris: Vec<&str> = documents.iter().map(|document| document.uri.as_str()).collect();
    let fail = |name: &str, detail: String| {
        remove_dir_logged(&inbox);
        release_uris(transfer.bridge, &uris);
        Some(Err(FileDialogError::CopyIn { name: name.to_owned(), detail }))
    };
    if let Err(err) = std::fs::create_dir_all(&inbox) {
        return fail(documents.first().map_or("", |document| document.name.as_str()), err.to_string());
    }
    let mut files: Vec<PickedFile> = Vec::with_capacity(documents.len());
    for (document, local_name) in documents.iter().zip(plan_file_copies(documents)) {
        if !transfer.still_wanted() {
            return withdrawn_during_copy(transfer, &inbox, &uris);
        }
        let path = inbox.join(local_name);
        if let Err(detail) = call_copy_to_file(transfer.bridge, &document.uri, &path) {
            return fail(&document.name, detail);
        }
        files.push(PickedFile { path, display_name: document.name.clone() });
    }
    release_uris(transfer.bridge, &uris);
    Some(Ok(Some(Picked::Files(files))))
}

/// Copies a picked tree into `saf_in/<request id>/<tree name>/` (`OpenFolder`).
fn copy_in_tree(transfer: &Transfer<'_>, tree: &PickedDocument) -> Option<DialogResult> {
    let inbox = transfer.dirs.inbox.join(transfer.request_id.to_string());
    let root = inbox.join(sanitize_file_name(&tree.name));
    let uris = [tree.uri.as_str()];
    let fail = |name: &str, detail: String| {
        remove_dir_logged(&inbox);
        release_uris(transfer.bridge, &uris);
        Some(Err(FileDialogError::CopyIn { name: name.to_owned(), detail }))
    };
    if let Err(err) = std::fs::create_dir_all(&root) {
        return fail(&tree.name, err.to_string());
    }
    let entries = match call_list_tree(transfer.bridge, &tree.uri).and_then(decode_tree_listing) {
        Ok(entries) => entries,
        Err(detail) => return fail(&tree.name, detail),
    };
    let plan = plan_tree_copy(&entries);
    for (step, entry) in plan.iter().zip(&entries) {
        if !transfer.still_wanted() {
            return withdrawn_during_copy(transfer, &inbox, &uris);
        }
        let path = root.join(&step.relative);
        let copied = if step.is_dir { std::fs::create_dir_all(&path).map_err(|err| err.to_string()) } else { call_copy_to_file(transfer.bridge, &step.uri, &path) };
        if let Err(detail) = copied {
            return fail(&entry.name, detail);
        }
    }
    release_uris(transfer.bridge, &uris);
    runtime_log::log_info(format!("[file-dialog] OpenFolder copied {} entries of '{}' into app storage", plan.len(), tree.name));
    Some(Ok(Some(Picked::Folder(root))))
}

/// The requester dropped its `PendingDialog` during a copy-in: undo and report nothing.
fn withdrawn_during_copy(transfer: &Transfer<'_>, inbox: &Path, uris: &[&str]) -> Option<DialogResult> {
    runtime_log::log_info(format!("[file-dialog] request {} was withdrawn during the copy-in; stopped", transfer.request_id));
    remove_dir_logged(inbox);
    release_uris(transfer.bridge, uris);
    None
}

/// A `SaveTarget` writing to `saf_out/<request id>/<staging name>`.
fn stage_file(transfer: &Transfer<'_>, document: PickedDocument, fallback_extension: Option<&str>) -> DialogResult {
    let staging_dir = transfer.dirs.outbox.join(transfer.request_id.to_string());
    if let Err(err) = std::fs::create_dir_all(&staging_dir) {
        release_uris(transfer.bridge, &[&document.uri]);
        return Err(FileDialogError::Backend { detail: format!("could not create the staging folder: {err}") });
    }
    let path = staging_dir.join(staging_file_name(&document.name, fallback_extension));
    Ok(Some(Picked::SaveTarget(SaveTarget { path, display_name: document.name, document: Some(FileDocument { uri: document.uri, staging_dir }) })))
}

/// A `FolderTarget` writing into `saf_out/<request id>/<tree name>/`, with the entries probe.
fn stage_folder(transfer: &Transfer<'_>, tree: PickedDocument) -> DialogResult {
    let staging_dir = transfer.dirs.outbox.join(transfer.request_id.to_string());
    let dir = staging_dir.join(sanitize_file_name(&tree.name));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        release_uris(transfer.bridge, &[&tree.uri]);
        return Err(FileDialogError::Backend { detail: format!("could not create the staging folder: {err}") });
    }
    let entries = call_tree_has_entries(transfer.bridge, &tree.uri);
    Ok(Some(Picked::FolderTarget(FolderTarget { dir, display_name: tree.name, document: Some(TreeDocument { uri: tree.uri, staging_dir, entries }) })))
}

/// Cleans up a result nobody will receive: local copies and staging dirs are removed, grants
/// still held for a save target are released.
fn discard_unclaimed(result: DialogResult) {
    let Ok(Some(picked)) = result else {
        return;
    };
    match picked {
        Picked::Files(files) => {
            if let Some(inbox) = files.first().and_then(|file| file.path.parent()) {
                remove_dir_logged(inbox);
            }
        }
        Picked::Folder(root) => {
            if let Some(inbox) = root.parent() {
                remove_dir_logged(inbox);
            }
        }
        Picked::SaveTarget(target) => {
            if let Some(document) = target.document {
                discard_document(&document.uri, &document.staging_dir);
            }
        }
        Picked::FolderTarget(target) => {
            if let Some(document) = target.document {
                discard_document(&document.uri, &document.staging_dir);
            }
        }
        Picked::DirectoryPath(_) => {}
    }
}

/// Releases the grant of an unused save destination and removes its staging dir.
fn discard_document(uri: &str, staging_dir: &Path) {
    match bridge_class() {
        Ok(bridge) => release_uris(bridge, &[uri]),
        Err(err) => runtime_log::log_warn(format!("[file-dialog] could not release an unused document grant: {err}")),
    }
    remove_dir_logged(staging_dir);
}

/// `SaveTarget::commit` on Android: streams `staged` to the document, then releases the grant
/// and removes the staging dir (also after a failure: the target is consumed either way).
///
/// # Errors
/// [`FileDialogError::Commit`] with the bridge's or the JNI detail.
pub(super) fn commit_file(staged: &Path, document: &FileDocument) -> Result<(), FileDialogError> {
    commit_with(&document.uri, &document.staging_dir, |bridge| call_stream_file_to_uri(bridge, staged, &document.uri))
}

/// `FolderTarget::commit` on Android: copies `staged_dir` into the tree (see the
/// `FolderTarget::commit` contract), then releases the grant and removes the staging dir.
///
/// # Errors
/// [`FileDialogError::Commit`] with the bridge's or the JNI detail.
pub(super) fn commit_folder(staged_dir: &Path, document: &TreeDocument) -> Result<(), FileDialogError> {
    commit_with(&document.uri, &document.staging_dir, |bridge| call_copy_dir_into_tree(bridge, staged_dir, &document.uri))
}

/// Runs one commit call, then releases the grant and removes the staging dir.
fn commit_with(uri: &str, staging_dir: &Path, publish: impl FnOnce(&Global<JClass<'static>>) -> Result<(), String>) -> Result<(), FileDialogError> {
    let bridge = bridge_class().map_err(|err| FileDialogError::Commit { detail: err.to_string() })?;
    let published = publish(bridge);
    release_uris(bridge, &[uri]);
    remove_dir_logged(staging_dir);
    published.map_err(|detail| FileDialogError::Commit { detail })
}

/// Removes `dir` recursively; a failure is only logged (the next process start purges it).
fn remove_dir_logged(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => runtime_log::log_warn(format!("[file-dialog] could not remove the transfer folder '{}': {err}", dir.display())),
    }
}

/// Releases the persisted grants of `uris`; failures are logged by Kotlin and here.
fn release_uris(bridge: &Global<JClass<'static>>, uris: &[&str]) {
    for uri in uris {
        let released = jni_ctx::with_env(|env, context| {
            let uri = JString::from_str(env, uri)?;
            env.call_static_method(bridge, jni_str!("releaseUri"), jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"), &[JValue::Object(context), JValue::Object(&uri)])?;
            Ok(())
        });
        if let Err(err) = released {
            runtime_log::log_warn(format!("[file-dialog] could not release a document grant: {err}"));
        }
    }
}

/// The `SafBridge` class, loaded once through the application class loader, with
/// `nativeOnResult` bound to this library (only a fully bound class is cached).
fn bridge_class() -> Result<&'static Global<JClass<'static>>, JniBridgeError> {
    if let Some(bridge) = BRIDGE.get() {
        return Ok(bridge);
    }
    let loaded = jni_ctx::load_app_class(SAF_BRIDGE_CLASS)?;
    register_natives(&loaded)?;
    Ok(BRIDGE.get_or_init(|| loaded))
}

/// Binds `SafBridge.nativeOnResult` explicitly (`RegisterNatives`). The exported JNI name
/// would be found by the JVM's own lookup too, but a binding that does not depend on the
/// dynamic-symbol export of the final `cdylib` is what lets a worker safely wait for the
/// result: an unbound native would leave it waiting forever.
fn register_natives(bridge: &Global<JClass<'static>>) -> Result<(), JniBridgeError> {
    jni_ctx::with_env(|env, _context| {
        // SAFETY: the pointer is the `extern "system"` function below; its parameters (the env,
        // the class of a STATIC method, jlong, jint, String[], String[], String) and its
        // jboolean return match the signature string exactly.
        let method = unsafe {
            NativeMethod::from_raw_parts(
                jni_str!("nativeOnResult"),
                jni_str!("(JI[Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Z"),
                Java_io_github_vasyanator_manhwastudio_platform_SafBridge_nativeOnResult as *mut std::ffi::c_void,
            )
        };
        // SAFETY: `nativeOnResult` is declared `@JvmStatic external` on the Kotlin object, i.e.
        // a static native method, matching the `JClass` second parameter (see above).
        unsafe { env.register_native_methods(bridge, &[method])? };
        Ok(())
    })
}

/// The transfer roots; the first call of the process resolves them and deletes whatever an
/// earlier process left there (its copies and stagings are unreachable now).
fn transfer_dirs() -> Result<&'static TransferDirs, JniBridgeError> {
    if let Some(dirs) = TRANSFER_DIRS.get() {
        return Ok(dirs);
    }
    let (cache_dir, files_dir) = jni_ctx::with_env(|env, context| {
        let cache = absolute_path_of(env, context, jni_str!("getCacheDir"))?;
        let files = absolute_path_of(env, context, jni_str!("getNoBackupFilesDir"))?;
        Ok((cache, files))
    })?;
    let dirs = TransferDirs { inbox: PathBuf::from(cache_dir).join(INBOX_DIR_NAME), outbox: PathBuf::from(files_dir).join(OUTBOX_DIR_NAME) };
    remove_dir_logged(&dirs.inbox);
    remove_dir_logged(&dirs.outbox);
    Ok(TRANSFER_DIRS.get_or_init(|| dirs))
}

/// `context.<getter>().getAbsolutePath()` for a `File`-returning `Context` getter.
fn absolute_path_of(env: &mut Env<'_>, context: &JObject<'_>, getter: &'static jni::strings::JNIStr) -> Result<String, JniError> {
    let file = env.call_method(context, getter, jni_sig!("()Ljava/io/File;"), &[])?.l()?;
    if file.is_null() {
        return Err(JniError::NullPtr("Context directory getter"));
    }
    let path = env.call_method(&file, jni_str!("getAbsolutePath"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
    required_string(env, path)
}

/// `SafBridge.launch`; `Err` carries the Kotlin refusal or the JNI failure.
fn call_launch(bridge: &Global<JClass<'static>>, request_id: u64, kind: i32, args: &LaunchArgs) -> Result<(), String> {
    let request_id = jlong::try_from(request_id).map_err(|_| format!("request id {request_id} does not fit a Java long"))?;
    let refused = jni_ctx::with_env(|env, context| {
        let placeholder = JString::from_str(env, "")?;
        let mime_types = JObjectArray::<JString>::new(env, args.mime_types.len(), &placeholder)?;
        for (index, mime) in args.mime_types.iter().enumerate() {
            let mime = JString::from_str(env, mime)?;
            mime_types.set_element(env, index, &mime)?;
        }
        let suggested = match &args.suggested_name {
            Some(name) => JObject::from(JString::from_str(env, name)?),
            None => JObject::null(),
        };
        let answer = env
            .call_static_method(
                bridge,
                jni_str!("launch"),
                jni_sig!("(Landroid/content/Context;JI[Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(context), JValue::Long(request_id), JValue::Int(kind), JValue::Object(&mime_types), JValue::Object(&suggested)],
            )?
            .l()?;
        Ok(optional_string(env, answer)?)
    });
    match refused {
        Ok(None) => Ok(()),
        Ok(Some(detail)) => Err(detail),
        Err(err) => Err(err.to_string()),
    }
}

/// A `(Context, String, String) -> String` helper of `SafBridge`: null = ok, else the error.
fn call_status_helper(bridge: &Global<JClass<'static>>, method: &'static jni::strings::JNIStr, first: &str, second: &str) -> Result<(), String> {
    let answer = jni_ctx::with_env(|env, context| {
        let first = JString::from_str(env, first)?;
        let second = JString::from_str(env, second)?;
        let answer = env
            .call_static_method(
                bridge,
                method,
                jni_sig!("(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(context), JValue::Object(&first), JValue::Object(&second)],
            )?
            .l()?;
        Ok(optional_string(env, answer)?)
    });
    match answer {
        Ok(None) => Ok(()),
        Ok(Some(detail)) => Err(detail),
        Err(err) => Err(err.to_string()),
    }
}

/// `SafBridge.copyToFile(uri, dest)`: one document into one local file (blocking).
fn call_copy_to_file(bridge: &Global<JClass<'static>>, uri: &str, dest: &Path) -> Result<(), String> {
    call_status_helper(bridge, jni_str!("copyToFile"), uri, &dest.to_string_lossy())
}

/// `SafBridge.streamFileToUri(src, uri)`: one local file into the document (blocking).
fn call_stream_file_to_uri(bridge: &Global<JClass<'static>>, src: &Path, uri: &str) -> Result<(), String> {
    call_status_helper(bridge, jni_str!("streamFileToUri"), &src.to_string_lossy(), uri)
}

/// `SafBridge.copyDirIntoTree(dir, treeUri)`: a local folder into the tree (blocking).
fn call_copy_dir_into_tree(bridge: &Global<JClass<'static>>, dir: &Path, tree_uri: &str) -> Result<(), String> {
    call_status_helper(bridge, jni_str!("copyDirIntoTree"), &dir.to_string_lossy(), tree_uri)
}

/// `SafBridge.listTree(treeUri)`: the raw listing for `decode_tree_listing`.
fn call_list_tree(bridge: &Global<JClass<'static>>, tree_uri: &str) -> Result<Vec<String>, String> {
    jni_ctx::with_env(|env, context| {
        let tree_uri = JString::from_str(env, tree_uri)?;
        let listing = env
            .call_static_method(
                bridge,
                jni_str!("listTree"),
                jni_sig!("(Landroid/content/Context;Ljava/lang/String;)[Ljava/lang/String;"),
                &[JValue::Object(context), JValue::Object(&tree_uri)],
            )?
            .l()?;
        if listing.is_null() {
            return Err(JniError::NullPtr("SafBridge.listTree").into());
        }
        let listing = JObjectArray::<JString>::cast_local(env, listing)?;
        Ok(read_string_array(env, &listing)?)
    })
    .map_err(|err| err.to_string())
}

/// `SafBridge.treeHasEntries(treeUri)` as the pick-time probe of a `FolderTarget`.
fn call_tree_has_entries(bridge: &Global<JClass<'static>>, tree_uri: &str) -> Result<bool, String> {
    let answer = jni_ctx::with_env(|env, context| {
        let tree_uri = JString::from_str(env, tree_uri)?;
        let answer = env
            .call_static_method(
                bridge,
                jni_str!("treeHasEntries"),
                jni_sig!("(Landroid/content/Context;Ljava/lang/String;)I"),
                &[JValue::Object(context), JValue::Object(&tree_uri)],
            )?
            .i()?;
        Ok(answer)
    });
    match answer {
        Ok(1) => Ok(true),
        Ok(0) => Ok(false),
        Ok(_) => Err("the chosen folder could not be listed (see logcat)".to_owned()),
        Err(err) => Err(err.to_string()),
    }
}

/// A possibly-null `String` result.
fn optional_string(env: &mut Env<'_>, object: JObject<'_>) -> Result<Option<String>, JniError> {
    if object.is_null() {
        return Ok(None);
    }
    required_string(env, object).map(Some)
}

/// A non-null `String` result.
fn required_string(env: &mut Env<'_>, object: JObject<'_>) -> Result<String, JniError> {
    if object.is_null() {
        return Err(JniError::NullPtr("expected a java.lang.String"));
    }
    let text = env.cast_local::<JString>(object)?;
    text.try_to_string(env)
}

/// Every element of a `String[]` (null array = empty, null element = empty string). Each
/// element's local reference is deleted at once, so long listings never fill the local frame.
fn read_string_array(env: &mut Env<'_>, array: &JObjectArray<'_, JString<'_>>) -> Result<Vec<String>, JniError> {
    if array.is_null() {
        return Ok(Vec::new());
    }
    let len = array.len(env)?;
    let mut items = Vec::with_capacity(len);
    for index in 0..len {
        let element = array.get_element(env, index)?;
        let text = if element.is_null() { String::new() } else { element.try_to_string(env)? };
        env.delete_local_ref(element);
        items.push(text);
    }
    Ok(items)
}

/// Reads the arguments of `nativeOnResult` into a typed outcome.
fn read_outcome(
    env: &mut Env<'_>,
    status: jint,
    uris: &JObjectArray<'_, JString<'_>>,
    names: &JObjectArray<'_, JString<'_>>,
    error: &JString<'_>,
) -> Result<BridgeOutcome, JniError> {
    let uris = read_string_array(env, uris)?;
    let names = read_string_array(env, names)?;
    let error = if error.is_null() { None } else { Some(error.try_to_string(env)?) };
    Ok(decode_outcome(status, uris, names, error))
}

/// Hands `outcome` to the worker waiting under `request_id`. Returns whether Rust now owns
/// the URI grants of the report (`false` = Kotlin must release them).
fn deliver(request_id: jlong, outcome: BridgeOutcome, read_ok: bool) -> bool {
    let Ok(request_id) = u64::try_from(request_id) else {
        runtime_log::log_warn(format!("[file-dialog] SAF result for an invalid request id {request_id}; discarded"));
        return false;
    };
    let Some(sender) = WAITERS.take(request_id) else {
        runtime_log::log_info(format!("[file-dialog] SAF result for unknown request {request_id}; discarded"));
        return false;
    };
    if sender.send(outcome).is_err() {
        runtime_log::log_warn(format!("[file-dialog] the worker of SAF request {request_id} is gone; result discarded"));
        return false;
    }
    read_ok
}

/// `SafBridge.nativeOnResult` (JNI name fixed by `SAF_BRIDGE_CLASS`; also bound explicitly by
/// `register_natives`). Called once per accepted
/// launch, on a Kotlin background thread; never blocks (a channel send) and never throws or
/// unwinds into the JVM: read failures become a `Failed` outcome for the waiting worker.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_vasyanator_manhwastudio_platform_SafBridge_nativeOnResult<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    request_id: jlong,
    status: jint,
    uris: JObjectArray<'local, JString<'local>>,
    names: JObjectArray<'local, JString<'local>>,
    error: JString<'local>,
) -> jboolean {
    let outcome = unowned_env
        .with_env(|env| -> Result<bool, JniError> {
            let (outcome, read_ok) = match read_outcome(env, status, &uris, &names, &error) {
                Ok(outcome) => (outcome, true),
                Err(err) => {
                    // Never return to Java with an exception pending from a failed read.
                    if env.exception_check() {
                        env.exception_describe();
                        env.exception_clear();
                    }
                    (BridgeOutcome::Failed(format!("could not read the picker result: {err}")), false)
                }
            };
            Ok(deliver(request_id, outcome, read_ok))
        })
        .into_outcome();
    match outcome {
        Outcome::Ok(accepted) => accepted,
        Outcome::Err(err) => {
            runtime_log::log_warn(format!("[file-dialog] SAF result {request_id} failed: {err}"));
            false
        }
        Outcome::Panic(_) => {
            runtime_log::log_error(format!("[file-dialog] panic while handling SAF result {request_id}"));
            false
        }
    }
}
