/*
File: clipboard/mod.rs

Purpose:
System clipboard READS for every platform: text, image, file list and HTML.

Key structures:
- ClipboardRgba: a clipboard image as tightly packed RGBA8.
- ClipboardError: typed error with a localized `user_message()`.

Key functions:
- read_text(), read_image(), read_file_list(), read_html().

Notes:
Blocking and short; callers may use them on the GUI thread exactly as they used arboard.
Writing to the clipboard goes through egui's `OutputCommand`, never through this module.
Backends: `desktop.rs` (arboard, a fresh handle per read), `unavailable.rs` (Android until the
JNI `ClipboardManager` backend lands, and wasm).
*/

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod desktop;
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
mod unavailable;

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use desktop as backend;
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
use unavailable as backend;

use std::fmt;
use std::path::PathBuf;

use crate::Unavailable;

/// A clipboard image: `width * height * 4` bytes of RGBA8, rows top to bottom. The backend
/// passes the platform's bytes through; validating the length is the consumer's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRgba {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Pixel bytes as delivered by the platform.
    pub rgba: Vec<u8>,
}

/// A clipboard read failure. `Display` is the localized [`Self::user_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// The platform (or this build) cannot read this kind of clipboard data.
    Unavailable(Unavailable),
    /// The clipboard holds nothing of the requested kind.
    NoContent,
    /// The platform clipboard failed (busy, conversion failure, unsupported configuration).
    Backend { detail: String },
}

impl ClipboardError {
    /// The i18n key of the message (the feature/reason sentences for `Unavailable`).
    const fn message_key(&self) -> &'static str {
        match self {
            ClipboardError::Unavailable(unavailable) => unavailable.feature.message_key(),
            ClipboardError::NoContent => "platform.clipboard.no_content_error",
            ClipboardError::Backend { .. } => "platform.clipboard.backend_error",
        }
    }

    /// Localized message for a status line or toast.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            ClipboardError::Unavailable(unavailable) => unavailable.user_message(),
            ClipboardError::NoContent => ms_i18n::resolve_key(self.message_key()).to_owned(),
            ClipboardError::Backend { detail } => {
                ms_i18n::interpolate(ms_i18n::resolve_key(self.message_key()), &[("detail", detail as &dyn fmt::Display)])
            }
        }
    }
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ClipboardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClipboardError::Unavailable(unavailable) => Some(unavailable),
            ClipboardError::NoContent | ClipboardError::Backend { .. } => None,
        }
    }
}

impl From<Unavailable> for ClipboardError {
    fn from(unavailable: Unavailable) -> Self {
        ClipboardError::Unavailable(unavailable)
    }
}

/// Reads plain text. `Ok(None)` = the clipboard holds no text.
///
/// # Errors
/// [`ClipboardError::Unavailable`] (`ClipboardText`) or [`ClipboardError::Backend`].
pub fn read_text() -> Result<Option<String>, ClipboardError> {
    backend::read_text()
}

/// Reads an image.
///
/// # Errors
/// [`ClipboardError::NoContent`] when there is no image, [`ClipboardError::Unavailable`]
/// (`ClipboardImage`) or [`ClipboardError::Backend`].
pub fn read_image() -> Result<ClipboardRgba, ClipboardError> {
    backend::read_image()
}

/// Reads a copied file list (e.g. files copied in a file manager).
///
/// # Errors
/// [`ClipboardError::NoContent`] when there is none, [`ClipboardError::Unavailable`]
/// (`ClipboardRichFormats`) or [`ClipboardError::Backend`].
pub fn read_file_list() -> Result<Vec<PathBuf>, ClipboardError> {
    backend::read_file_list()
}

/// Reads HTML. `Ok(None)` = the clipboard holds no HTML.
///
/// # Errors
/// [`ClipboardError::Unavailable`] (`ClipboardRichFormats`) or [`ClipboardError::Backend`].
pub fn read_html() -> Result<Option<String>, ClipboardError> {
    backend::read_html()
}

#[cfg(test)]
mod tests {
    use super::ClipboardError;
    use crate::test_catalog::{assert_key_in_shipped_catalogs, install_english};
    use crate::{PlatformFeature, Unavailable, UnavailableReason};

    #[test]
    fn every_message_key_is_shipped() {
        for error in [ClipboardError::NoContent, ClipboardError::Backend { detail: "x".to_owned() }] {
            assert_key_in_shipped_catalogs(error.message_key());
        }
    }

    #[test]
    fn messages_carry_details_and_unavailable_text() {
        install_english();
        let backend = ClipboardError::Backend { detail: "occupied-by-test".to_owned() };
        assert!(backend.user_message().contains("occupied-by-test"));
        let unavailable = Unavailable::new(PlatformFeature::ClipboardImage, UnavailableReason::NotOnThisPlatform);
        assert_eq!(ClipboardError::from(unavailable).user_message(), unavailable.user_message());
        assert!(!ClipboardError::NoContent.user_message().is_empty());
    }
}
