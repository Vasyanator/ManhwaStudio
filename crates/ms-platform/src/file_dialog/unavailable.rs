/*
File: file_dialog/unavailable.rs

Purpose:
File-dialog backend of targets without any dialog (wasm32). `request()` already answers
`Unavailable(FilePicker)` from the capability table before reaching a backend; this backend
exists so the dispatch compiles and gives the same typed answer if it is ever reached.
*/

use super::{Completion, DropGuard, FileDialogError, FileDialogRequest};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

/// Resolves the request at once with `Unavailable(FilePicker)` through its completion (which
/// also releases the in-flight flag and wakes the requester).
///
/// # Errors
/// Never: the failure is delivered as the dialog result.
#[expect(clippy::unnecessary_wraps, reason = "shares the backend dispatch signature")]
pub(super) fn launch(_request: FileDialogRequest, completion: Completion) -> Result<Option<DropGuard>, FileDialogError> {
    completion.finish(Err(Unavailable::new(PlatformFeature::FilePicker, UnavailableReason::NotOnThisPlatform).into()));
    Ok(None)
}
