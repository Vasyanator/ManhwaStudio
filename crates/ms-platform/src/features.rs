/*
File: features.rs

Purpose:
The platform capability table: the ONE place that decides whether a platform offers a
feature, and the typed error a gated entry point returns when it does not.

Key structures:
- PlatformFeature: every capability that differs between desktop, Android and wasm.
- UnavailableReason: why a feature is missing (platform vs. missing Android bridge).
- Unavailable: the typed error, with a localized `user_message()`.

Key functions:
- PlatformFeature::is_available(): `const fn` over `cfg!`, so a gated branch folds away.
- PlatformFeature::require(): the same decision as a `Result`.

Notes:
"Desktop" means every target that is neither wasm32 nor Android (Linux, Windows, macOS). Code
stays compiled on every target; only dependencies without an Android/wasm backend are gated by
Cargo target cfg (see the crate manifest).
*/

use std::fmt;

/// `true` on the desktop targets (neither wasm32 nor Android).
const DESKTOP: bool = cfg!(not(any(target_arch = "wasm32", target_os = "android")));
/// `true` on Android.
const ANDROID: bool = cfg!(target_os = "android");

/// A capability that differs between platforms. [`PlatformFeature::is_available`] is the single
/// owner of the per-platform answer; callers never re-derive it from `cfg` themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformFeature {
    /// The out-of-process Python AI backend and everything that spawns Python.
    PythonBackend,
    /// Installer, self-update, update check and the installed-copy prompts.
    InstalledCopyManagement,
    /// The in-process native ONNX Runtime (download and load of the dylib).
    NativeOnnxRuntime,
    /// The desktop region screen-capture overlay.
    ScreenCapture,
    /// RAR / 7z extraction through external command-line tools.
    ExternalArchiveTools,
    /// The external waifu2x upscaler executable.
    Waifu2xUpscaler,
    /// The OS credential store for AI API keys and the Hugging Face token.
    CredentialStore,
    /// File open/save dialogs (`file_dialog`). On Android a request may still resolve to
    /// [`UnavailableReason::BridgeMissing`] at runtime.
    FilePicker,
    /// A picked directory PATH that is kept and used in place (install dir, model dirs, batch
    /// output). Android's SAF yields tree URIs, never usable paths.
    PersistentDirectoryPicker,
    /// A native OS message box (used before any window exists).
    NativeMessageBox,
    /// Reading plain text from the system clipboard.
    ClipboardText,
    /// Reading an image from the system clipboard.
    ClipboardImage,
    /// Reading a file list or HTML from the system clipboard.
    ClipboardRichFormats,
    /// Choosing the app-specific storage volume (internal / SD card) for projects.
    AppStorageVolumes,
    /// The Android "All files access" permission for a shared projects folder.
    AllFilesAccess,
}

impl PlatformFeature {
    /// Every feature, in declaration order (tests and diagnostics iterate it).
    pub const ALL: [PlatformFeature; 15] = [
        PlatformFeature::PythonBackend,
        PlatformFeature::InstalledCopyManagement,
        PlatformFeature::NativeOnnxRuntime,
        PlatformFeature::ScreenCapture,
        PlatformFeature::ExternalArchiveTools,
        PlatformFeature::Waifu2xUpscaler,
        PlatformFeature::CredentialStore,
        PlatformFeature::FilePicker,
        PlatformFeature::PersistentDirectoryPicker,
        PlatformFeature::NativeMessageBox,
        PlatformFeature::ClipboardText,
        PlatformFeature::ClipboardImage,
        PlatformFeature::ClipboardRichFormats,
        PlatformFeature::AppStorageVolumes,
        PlatformFeature::AllFilesAccess,
    ];

    /// Whether the current build target offers this feature. Decided at compile time from
    /// `cfg!`, so a branch on it is folded away; the one owner of the capability table.
    #[must_use]
    pub const fn is_available(self) -> bool {
        match self {
            // Desktop has rfd / arboard; Android gets SAF and JNI ClipboardManager backends.
            PlatformFeature::FilePicker | PlatformFeature::ClipboardText => DESKTOP || ANDROID,
            // Android-only storage model (app-specific external dirs, MANAGE_EXTERNAL_STORAGE).
            PlatformFeature::AppStorageVolumes | PlatformFeature::AllFilesAccess => ANDROID,
            PlatformFeature::PythonBackend
            | PlatformFeature::InstalledCopyManagement
            | PlatformFeature::NativeOnnxRuntime
            | PlatformFeature::ScreenCapture
            | PlatformFeature::ExternalArchiveTools
            | PlatformFeature::Waifu2xUpscaler
            | PlatformFeature::CredentialStore
            | PlatformFeature::PersistentDirectoryPicker
            | PlatformFeature::NativeMessageBox
            | PlatformFeature::ClipboardImage
            | PlatformFeature::ClipboardRichFormats => DESKTOP,
        }
    }

    /// [`Self::is_available`] as a `Result`, for `?` at the top of a gated entry point.
    ///
    /// # Errors
    /// [`Unavailable`] with [`UnavailableReason::NotOnThisPlatform`] when the feature is absent.
    pub const fn require(self) -> Result<(), Unavailable> {
        if self.is_available() {
            Ok(())
        } else {
            Err(Unavailable { feature: self, reason: UnavailableReason::NotOnThisPlatform })
        }
    }

    /// The i18n key of the sentence naming this feature as unavailable.
    pub(crate) const fn message_key(self) -> &'static str {
        match self {
            PlatformFeature::PythonBackend => "platform.unavailable.python_backend_error",
            PlatformFeature::InstalledCopyManagement => "platform.unavailable.installed_copy_management_error",
            PlatformFeature::NativeOnnxRuntime => "platform.unavailable.native_onnx_runtime_error",
            PlatformFeature::ScreenCapture => "platform.unavailable.screen_capture_error",
            PlatformFeature::ExternalArchiveTools => "platform.unavailable.external_archive_tools_error",
            PlatformFeature::Waifu2xUpscaler => "platform.unavailable.waifu2x_upscaler_error",
            PlatformFeature::CredentialStore => "platform.unavailable.credential_store_error",
            PlatformFeature::FilePicker => "platform.unavailable.file_picker_error",
            PlatformFeature::PersistentDirectoryPicker => "platform.unavailable.persistent_directory_picker_error",
            PlatformFeature::NativeMessageBox => "platform.unavailable.native_message_box_error",
            PlatformFeature::ClipboardText => "platform.unavailable.clipboard_text_error",
            PlatformFeature::ClipboardImage => "platform.unavailable.clipboard_image_error",
            PlatformFeature::ClipboardRichFormats => "platform.unavailable.clipboard_rich_formats_error",
            PlatformFeature::AppStorageVolumes => "platform.unavailable.app_storage_volumes_error",
            PlatformFeature::AllFilesAccess => "platform.unavailable.all_files_access_error",
        }
    }
}

/// Why a feature is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnavailableReason {
    /// The platform does not offer the feature at all (the capability table says no).
    NotOnThisPlatform,
    /// The platform offers it, but this build lacks the Android bridge it needs (the Kotlin
    /// class or its Rust launch path is not part of the build yet).
    BridgeMissing,
}

impl UnavailableReason {
    /// The i18n key of the sentence explaining the reason.
    pub(crate) const fn message_key(self) -> &'static str {
        match self {
            UnavailableReason::NotOnThisPlatform => "platform.unavailable.not_on_this_platform_hint",
            UnavailableReason::BridgeMissing => "platform.unavailable.bridge_missing_hint",
        }
    }
}

/// A gated feature is not available here. `Display` is the localized [`Self::user_message`],
/// ready for a status line or toast; log it with `{:?}` for the typed details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Unavailable {
    /// The missing capability.
    pub feature: PlatformFeature,
    /// Why it is missing.
    pub reason: UnavailableReason,
}

impl Unavailable {
    /// Builds the error for `feature` with an explicit `reason`.
    #[must_use]
    pub const fn new(feature: PlatformFeature, reason: UnavailableReason) -> Self {
        Self { feature, reason }
    }

    /// Localized two-sentence message: the feature sentence, then the reason sentence.
    #[must_use]
    pub fn user_message(&self) -> String {
        format!(
            "{} {}",
            ms_i18n::resolve_key(self.feature.message_key()),
            ms_i18n::resolve_key(self.reason.message_key())
        )
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for Unavailable {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_catalog::assert_key_in_shipped_catalogs;

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_desktop_table() {
        for feature in PlatformFeature::ALL {
            let expected = !matches!(feature, PlatformFeature::AppStorageVolumes | PlatformFeature::AllFilesAccess);
            assert_eq!(feature.is_available(), expected, "{feature:?}");
            assert_eq!(feature.require().is_ok(), expected, "{feature:?}");
        }
    }

    #[test]
    fn require_reports_not_on_this_platform() {
        for feature in PlatformFeature::ALL {
            if let Err(err) = feature.require() {
                assert_eq!(err, Unavailable::new(feature, UnavailableReason::NotOnThisPlatform));
            }
        }
    }

    #[test]
    fn every_message_key_is_shipped_in_en_and_ru() {
        for feature in PlatformFeature::ALL {
            assert_key_in_shipped_catalogs(feature.message_key());
        }
        for reason in [UnavailableReason::NotOnThisPlatform, UnavailableReason::BridgeMissing] {
            assert_key_in_shipped_catalogs(reason.message_key());
        }
    }

    #[test]
    fn user_message_names_feature_and_reason() {
        crate::test_catalog::install_english();
        let err = Unavailable::new(PlatformFeature::FilePicker, UnavailableReason::BridgeMissing);
        let message = err.user_message();
        assert!(message.contains(ms_i18n::resolve_key(PlatformFeature::FilePicker.message_key())));
        assert!(message.contains(ms_i18n::resolve_key(UnavailableReason::BridgeMissing.message_key())));
        assert_eq!(err.to_string(), message);
        assert!(!message.contains("platform."), "a catalog miss would show the raw key: {message}");
    }
}
