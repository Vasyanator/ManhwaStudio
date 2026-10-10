/*
File: clipboard/unavailable.rs

Purpose:
Clipboard backend of targets without a system clipboard reader in this build: wasm32, and
Android until the JNI `ClipboardManager` text backend replaces the `read_text` dispatch there.

Notes:
The reason follows the capability table: a feature the platform has (Android text) reports
`BridgeMissing` (this build lacks the bridge), any other `NotOnThisPlatform`.
*/

use std::path::PathBuf;

use super::{ClipboardError, ClipboardRgba};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

pub(super) fn read_text() -> Result<Option<String>, ClipboardError> {
    Err(unavailable(PlatformFeature::ClipboardText))
}

pub(super) fn read_image() -> Result<ClipboardRgba, ClipboardError> {
    Err(unavailable(PlatformFeature::ClipboardImage))
}

pub(super) fn read_file_list() -> Result<Vec<PathBuf>, ClipboardError> {
    Err(unavailable(PlatformFeature::ClipboardRichFormats))
}

pub(super) fn read_html() -> Result<Option<String>, ClipboardError> {
    Err(unavailable(PlatformFeature::ClipboardRichFormats))
}

fn unavailable(feature: PlatformFeature) -> ClipboardError {
    let reason = if feature.is_available() { UnavailableReason::BridgeMissing } else { UnavailableReason::NotOnThisPlatform };
    Unavailable::new(feature, reason).into()
}
