/*
File: crates/ms-window-geometry/src/identity.rs

Purpose:
The one owner of the program's native WINDOW IDENTITY: the app id every main-window viewport
carries and the window icon. Every window that should be grouped with the main window by the
desktop (taskbar, dock, window switcher) takes both from here instead of spelling its own.

Key items:
- `MAIN_WINDOW_APP_ID`, `TEST_WINDOW_APP_ID`: the app ids of a normal and a `--test-launcher` run.
- `app_id(test_mode)`: picks between them.
- `app_icon()`: the embedded icon, decoded once per process and shared.
- `with_identity(builder, test_mode)`: stamps app id + icon on a `ViewportBuilder`.

Notes:
On Linux the main app id IS the desktop-entry id (`manhwastudio_rs.desktop`,
`StartupWMClass=manhwastudio_rs`, written by `ms_os_integration::linux`), so the window started
from the application menu matches its menu entry and icon (X11 `WM_CLASS`, Wayland `app_id`).
The separate prompt windows of `src/lib.rs` (update check, basic launcher, missing environment)
and the installer keep their own app ids by design; they take only the icon from here.
*/

use std::sync::{Arc, OnceLock};

/// Native app id of the main window (launcher and studio share one window identity). On Linux
/// it is the desktop-entry id; elsewhere a program-specific id.
#[cfg(target_os = "linux")]
pub const MAIN_WINDOW_APP_ID: &str = "manhwastudio_rs";
/// Native app id of the main window (launcher and studio share one window identity). On Linux
/// it is the desktop-entry id; elsewhere a program-specific id.
#[cfg(not(target_os = "linux"))]
pub const MAIN_WINDOW_APP_ID: &str = "manhwastudio_rs.main";
/// Native app id of a `--test-launcher` run, kept distinct so a test window is never grouped
/// with (or mistaken for) a real one.
pub const TEST_WINDOW_APP_ID: &str = "manhwastudio_rs.launcher_test";

/// The embedded icon in its two encodings: the multi-size ICO is preferred, the 512 px PNG is
/// the fallback when the ICO cannot be decoded.
const EMBEDDED_APP_ICON_ICO: &[u8] = include_bytes!("../../../app_icon.ico");
const EMBEDDED_APP_ICON_PNG: &[u8] = include_bytes!("../../../app_icon_512.png");

/// The app id of a main-window viewport: [`TEST_WINDOW_APP_ID`] in test mode, else
/// [`MAIN_WINDOW_APP_ID`].
#[must_use]
pub fn app_id(test_mode: bool) -> &'static str {
    if test_mode { TEST_WINDOW_APP_ID } else { MAIN_WINDOW_APP_ID }
}

/// The program's window icon, decoded on the first call and shared afterwards (viewport
/// builders are rebuilt every frame for immediate child windows, so decoding per call would
/// redo the image work each frame). `None` when neither embedded encoding decodes; that is
/// logged once and the window then simply opens without an icon.
#[must_use]
pub fn app_icon() -> Option<Arc<egui::IconData>> {
    static ICON: OnceLock<Option<Arc<egui::IconData>>> = OnceLock::new();
    ICON.get_or_init(|| decode_icon().map(Arc::new)).clone()
}

/// Stamps the program's window identity on `builder`: the app id for `test_mode` and the
/// icon (when it decodes). Used by every window that belongs to the main window's group.
#[must_use]
pub fn with_identity(builder: egui::ViewportBuilder, test_mode: bool) -> egui::ViewportBuilder {
    let builder = builder.with_app_id(app_id(test_mode));
    match app_icon() {
        Some(icon) => builder.with_icon(icon),
        None => builder,
    }
}

/// Decodes the embedded icon into RGBA: the ICO first, then the PNG.
fn decode_icon() -> Option<egui::IconData> {
    let image = match image::load_from_memory(EMBEDDED_APP_ICON_ICO) {
        Ok(image) => image,
        Err(ico_err) => match image::load_from_memory(EMBEDDED_APP_ICON_PNG) {
            Ok(image) => {
                ms_log::runtime_log::log_warn(format!("[window-identity] embedded ICO icon did not decode; using the PNG fallback; error={ico_err}"));
                image
            }
            Err(png_err) => {
                ms_log::runtime_log::log_error(format!("[window-identity] neither embedded icon decodes; windows open without an icon; ico_error={ico_err}; png_error={png_err}"));
                return None;
            }
        },
    };
    let rgba = image.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    Some(egui::IconData { rgba: rgba.into_raw(), width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Linux the main window carries the desktop-entry id, so the menu entry's
    /// `StartupWMClass` matches it; other systems and the test mode keep their own ids.
    #[test]
    fn app_id_per_target_and_mode() {
        if cfg!(target_os = "linux") {
            assert_eq!(app_id(false), "manhwastudio_rs");
        } else {
            assert_eq!(app_id(false), "manhwastudio_rs.main");
        }
        assert_eq!(app_id(true), "manhwastudio_rs.launcher_test");
    }

    /// The embedded icon decodes into a consistent RGBA buffer, and later calls share the
    /// same decoded instance.
    #[test]
    fn app_icon_decodes_once_into_rgba() {
        let Some(icon) = app_icon() else {
            panic!("the embedded app icon must decode");
        };
        assert!(icon.width > 0 && icon.height > 0);
        let expected_len = usize::try_from(u64::from(icon.width) * u64::from(icon.height) * 4).unwrap_or(usize::MAX);
        assert_eq!(icon.rgba.len(), expected_len);
        let Some(again) = app_icon() else {
            panic!("the cached app icon must stay available");
        };
        assert!(Arc::ptr_eq(&icon, &again));
    }

    /// `with_identity` sets both the app id and the icon.
    #[test]
    fn with_identity_stamps_app_id_and_icon() {
        let builder = with_identity(egui::ViewportBuilder::default(), true);
        assert_eq!(builder.app_id.as_deref(), Some(TEST_WINDOW_APP_ID));
        assert!(builder.icon.is_some());
    }
}
