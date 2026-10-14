/*
File: lib.rs

Purpose:
Crate root of `ms-platform`, the GUI-free owner of every "what does this platform offer"
decision and of the OS services that differ per platform (desktop, Android, wasm).

Main responsibilities:
- the capability table: `PlatformFeature::is_available()` (the one owner) and the typed
  `Unavailable` error every gated entry point returns;
- the async-shaped file-dialog seam (`file_dialog`): rfd on desktop, Storage Access Framework
  on Android (Kotlin `SafBridge`), typed `Unavailable` on wasm;
- system clipboard reads (`clipboard`): arboard on desktop, JNI text on Android, typed
  `Unavailable` for the rest;
- images handed over by other apps (`incoming`): the Android Share / "Open with" inbox (Kotlin
  `IncomingImages`), "nothing incoming" on every other target;
- the pre-window error box (`message::show_error_blocking`);
- the installed package's native library directory (`native_libs`, Android only): where the
  onnxruntime library bundled in the APK is loaded from;
- the Android credential store (`credentials`, Kotlin `CredentialBridge`): API keys and tokens
  encrypted under an AndroidKeyStore key. Android only; desktop consumers keep `keyring`.

Key structures:
- PlatformFeature, Unavailable, UnavailableReason (`features.rs`)
- file_dialog::{FileDialogRequest, PendingDialog, Picked, SaveTarget, FolderTarget}
- clipboard::{ClipboardRgba, ClipboardError}
- incoming::{IncomingBatch, IncomingSkip, IncomingError}
- credentials::CredentialError (Android)
- native_libs::NativeLibraryDirError (Android)

Notes:
Holds no egui types: callers turn a dialog result into UI themselves and pass a repaint
callback (`file_dialog::Waker`). The only crate that names `rfd` / `arboard`; see
`MODULE_README.md` for the per-target backend map.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

mod features;

pub mod clipboard;
// The credential store seam has only an Android backend (desktop consumers use `keyring`
// directly, the web has no store); its pure half is also compiled for host tests.
#[cfg(any(target_os = "android", test))]
pub mod credentials;
pub mod file_dialog;
pub mod incoming;
pub mod message;
// The installed package's native library directory (where the APK's bundled onnxruntime
// lives). Android only: no other target has a package library folder to report.
#[cfg(target_os = "android")]
pub mod native_libs;

// Android JNI plumbing. `saf.rs` (the pure half of the SAF bridge) is pure Rust, so it is also
// compiled for host tests; everything that touches the JVM is Android-only inside.
#[cfg(any(target_os = "android", test))]
mod android;

pub use features::{PlatformFeature, Unavailable, UnavailableReason};

/// Test helpers for the localized messages. Key presence is read from the embedded sources;
/// the only locale any test of this crate installs is English, so concurrent installs from
/// parallel tests always leave the same catalog active.
#[cfg(test)]
mod test_catalog {
    /// Installs the embedded English catalog as the active one.
    pub(crate) fn install_english() {
        ms_i18n::set_locale(&ms_i18n::LocaleTag::english()).unwrap_or_else(|err| panic!("embedded en catalog: {err}"));
    }

    /// Panics (test failure) unless `key` is a non-empty string in both `en` and `ru`.
    pub(crate) fn assert_key_in_shipped_catalogs(key: &str) {
        for tag in ["en", "ru"] {
            let source = ms_i18n::embedded_locales()
                .iter()
                .find(|(embedded, _)| *embedded == tag)
                .map_or_else(|| panic!("no embedded catalog for {tag}"), |(_, source)| *source);
            let catalog: serde_json::Value = serde_json::from_str(source).unwrap_or_else(|err| panic!("{tag}.json does not parse: {err}"));
            let text = catalog.get(key).and_then(serde_json::Value::as_str).unwrap_or_default();
            assert!(!text.trim().is_empty(), "key {key} is missing or empty in {tag}.json");
        }
    }
}
