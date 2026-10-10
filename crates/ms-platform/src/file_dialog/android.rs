/*
File: file_dialog/android.rs

Purpose:
Android file-dialog backend over the Storage Access Framework bridge.

Key functions:
- launch(): registers the request in the pending registry and starts a `platform-bridge`
  worker; the returned guard withdraws the registration when the `PendingDialog` is dropped.

Notes:
The Kotlin `SafBridge` and its Rust launch / result path do not exist yet (WP9). Until they do,
the worker only probes for the bridge class through the application class loader (never JNI
on the GUI thread) and resolves EVERY request to `Unavailable { FilePicker, BridgeMissing }`:
whether or not the class loads, this build cannot launch it. `PickDirectoryPath` never reaches
this backend (`PersistentDirectoryPicker` is unavailable on Android).
*/

use ms_log::runtime_log;

use super::{Completion, DialogResult, DropGuard, FileDialogError, FileDialogRequest};
use crate::android::jni_ctx::{self, JniBridgeError};
use crate::android::saf::{PendingRegistry, SAF_BRIDGE_CLASS};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

/// Requests waiting for the bridge, keyed by the id passed to Kotlin.
static PENDING: PendingRegistry<Completion> = PendingRegistry::new();

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
    let id = PENDING.register(completion);
    let spawned = std::thread::Builder::new().name("platform-bridge".to_owned()).spawn(move || {
        let result = resolve_without_bridge(&request);
        match PENDING.take(id) {
            Some(completion) => completion.finish(result),
            None => runtime_log::log_info(format!("[file-dialog] {:?} request {id} was withdrawn; late result discarded", request.kind())),
        }
    });
    match spawned {
        // Detached on purpose: the result travels through the registry, never `join`.
        Ok(_detached) => Ok(Some(Box::new(Registration(id)))),
        Err(err) => {
            drop(PENDING.take(id));
            Err(FileDialogError::Backend { detail: err.to_string() })
        }
    }
}

/// Probes for the SAF bridge class and answers the request without it (see the file header).
fn resolve_without_bridge(request: &FileDialogRequest) -> DialogResult {
    let bridge_missing = || Err(Unavailable::new(PlatformFeature::FilePicker, UnavailableReason::BridgeMissing).into());
    match jni_ctx::load_app_class(SAF_BRIDGE_CLASS) {
        Ok(_class) => {
            runtime_log::log_warn(format!("[file-dialog] {:?}: the SAF bridge class is present, but this build has no launch path for it", request.kind()));
            bridge_missing()
        }
        Err(JniBridgeError::BridgeMissing { .. }) => bridge_missing(),
        Err(err) => Err(FileDialogError::Backend { detail: err.to_string() }),
    }
}
