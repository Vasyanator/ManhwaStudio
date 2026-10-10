/*
File: message.rs

Purpose:
The native error box used before any application window exists (startup failures).

Key functions:
- show_error_blocking(): rfd `MessageDialog` (Error level, one OK button) on desktop; typed
  `Unavailable(NativeMessageBox)` elsewhere, so the caller logs the message instead.

Notes:
Blocks until the user closes the box. Never call it while an egui window is running: in-app
errors belong in the UI.
*/

use crate::Unavailable;
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
use crate::{PlatformFeature, UnavailableReason};

/// Shows a blocking native error box with `title` and `text`.
///
/// # Errors
/// [`Unavailable`] for `NativeMessageBox` on targets without native message boxes.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn show_error_blocking(title: &str, text: &str) -> Result<(), Unavailable> {
    // The box has a single OK button, so the clicked-button value carries no information.
    let _clicked: rfd::MessageDialogResult = rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .set_level(rfd::MessageLevel::Error)
        .show();
    Ok(())
}

/// Shows a blocking native error box with `title` and `text`.
///
/// # Errors
/// Always [`Unavailable`] for `NativeMessageBox`: this target has no native message box.
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn show_error_blocking(_title: &str, _text: &str) -> Result<(), Unavailable> {
    Err(Unavailable::new(PlatformFeature::NativeMessageBox, UnavailableReason::NotOnThisPlatform))
}
