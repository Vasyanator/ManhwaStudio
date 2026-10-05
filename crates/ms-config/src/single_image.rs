/*
File: crates/ms-config/src/single_image.rs

Purpose:
Names, data and the one setting of the single-image editing mode (open one picture file,
edit it in the studio, write it back): which input files it accepts, where its throwaway
scratch chapters live, and the `SingleImage` section of `user_config`.

Key items:
- `ImageFileType` / `INPUT_FILE_TYPES` / `input_extensions()`: the ONE table of readable input
  types. Consumed by the launcher picker filter, the Linux `.desktop` `MimeType=` line, the
  Windows "Open with" `SupportedTypes`, and the CLI error text; a test in `ms-project` asserts
  every entry decodes.
- `scratch_base()` (native only) + `SCRATCH_MARKER_FILE` / `SCRATCH_LOCK_FILE`: the per-user
  scratch root under the OS temp directory and the two per-session file names the startup sweep
  keys on.
- `SINGLE_IMAGE_SECTION` / `SINGLE_IMAGE_JPEG_QUALITY_KEY` / `JPEG_QUALITY_*`: the setting's
  location, bounds and default (`user_config_defaults` in `lib.rs` names them).
- `jpeg_quality_from()`: pure, clamping reader. `save_jpeg_quality()`: one targeted
  `ms_docstore::update` of the section (blocking I/O, worker threads only).

Notes:
The key is Rust-only: no Python code reads it, so it is outside the `PROTOCOL_VERSION`
contract (which covers the STORAGE semantics of `user_config`, not individual Rust keys).
*/

use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use serde_json::Value;

use crate::{edit_section, update_user_config_root, user_config_error_message};

/// One readable input image type: its file extensions (lowercase, without the dot) and its
/// IANA / freedesktop MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFileType {
    /// Lowercase extensions without the leading dot; the first one is the canonical spelling.
    pub extensions: &'static [&'static str],
    /// MIME type used by the Linux desktop entry and picker filters.
    pub mime: &'static str,
}

/// Every image type the single-image mode accepts as input. AVIF is deliberately absent: its
/// decoder sits behind the `image` crate's `avif-native` feature, which the build does not enable.
pub const INPUT_FILE_TYPES: &[ImageFileType] = &[
    ImageFileType { extensions: &["png"], mime: "image/png" },
    ImageFileType { extensions: &["jpg", "jpeg", "jpe"], mime: "image/jpeg" },
    ImageFileType { extensions: &["webp"], mime: "image/webp" },
    ImageFileType { extensions: &["bmp"], mime: "image/bmp" },
    ImageFileType { extensions: &["tif", "tiff"], mime: "image/tiff" },
    ImageFileType { extensions: &["gif"], mime: "image/gif" },
    ImageFileType { extensions: &["tga"], mime: "image/x-tga" },
    ImageFileType { extensions: &["qoi"], mime: "image/qoi" },
];

/// Every extension of [`INPUT_FILE_TYPES`], in table order (lowercase, no dot).
pub fn input_extensions() -> impl Iterator<Item = &'static str> {
    INPUT_FILE_TYPES.iter().flat_map(|file_type| file_type.extensions.iter().copied())
}

/// Name of the directory under the OS temp directory that holds every scratch session.
#[cfg(not(target_arch = "wasm32"))]
const SCRATCH_DIR_NAME: &str = "manhwastudio-single-image";

/// Marker file at a session root. Only directories carrying it are ever swept, so a foreign
/// directory under [`scratch_base`] is never deleted.
pub const SCRATCH_MARKER_FILE: &str = ".ms-single-image";

/// Lock file at a session root, held with an exclusive `File::try_lock` for the session's whole
/// life. A sweep that acquires it knows the owning process is gone.
pub const SCRATCH_LOCK_FILE: &str = ".lock";

/// Longest user-name suffix [`scratch_dir_name`] keeps (bytes of the sanitized ASCII name).
#[cfg(not(target_arch = "wasm32"))]
const SCRATCH_USER_SUFFIX_MAX: usize = 32;

/// Root directory of all single-image scratch sessions of the CURRENT user:
/// `std::env::temp_dir()/manhwastudio-single-image-<user>` on Unix, where `/tmp` is shared by every
/// account (one shared directory would be owned by whoever created it first, and the mode would
/// fail for everyone else). `<user>` is `$USER`, else `$LOGNAME`, sanitized by
/// [`scratch_dir_name`]; with neither set the unsuffixed name is used. On Windows `temp_dir` is
/// already per-user, so the name stays unsuffixed. Pure path computation (environment reads only),
/// no file I/O. Native only: `std::env::temp_dir` has no meaning on wasm, and the mode itself is
/// compiled out there.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn scratch_base() -> PathBuf {
    #[cfg(unix)]
    let user = std::env::var_os("USER").or_else(|| std::env::var_os("LOGNAME")).map(|name| name.to_string_lossy().into_owned());
    #[cfg(not(unix))]
    let user: Option<String> = None;
    std::env::temp_dir().join(scratch_dir_name(user.as_deref()))
}

/// The scratch base's directory name for `user`: `manhwastudio-single-image-<sanitized user>`, or
/// the bare name when `user` is `None` or sanitizes to nothing. Sanitizing keeps ASCII
/// alphanumerics, `-` and `_`, maps every other character to `_` (so the name never contains a
/// separator or a dot-only component) and truncates to `SCRATCH_USER_SUFFIX_MAX` bytes.
#[cfg(not(target_arch = "wasm32"))]
fn scratch_dir_name(user: Option<&str>) -> String {
    let suffix: String = user
        .unwrap_or_default()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .take(SCRATCH_USER_SUFFIX_MAX)
        .collect();
    if suffix.is_empty() { SCRATCH_DIR_NAME.to_owned() } else { format!("{SCRATCH_DIR_NAME}-{suffix}") }
}

/// `user_config` section of the single-image mode.
pub const SINGLE_IMAGE_SECTION: &str = "SingleImage";
/// Key of the JPEG encode quality inside [`SINGLE_IMAGE_SECTION`].
pub const SINGLE_IMAGE_JPEG_QUALITY_KEY: &str = "jpeg_quality";
/// Default JPEG quality (user decision).
pub const JPEG_QUALITY_DEFAULT: u8 = 95;
/// Lowest accepted JPEG quality.
pub const JPEG_QUALITY_MIN: u8 = 1;
/// Highest accepted JPEG quality.
pub const JPEG_QUALITY_MAX: u8 = 100;

/// Reads `SingleImage.jpeg_quality` from a user-config root. An integer is clamped into
/// `JPEG_QUALITY_MIN..=JPEG_QUALITY_MAX`; a missing section or key, or a non-integer value,
/// yields [`JPEG_QUALITY_DEFAULT`].
#[must_use]
pub fn jpeg_quality_from(settings: &Value) -> u8 {
    let raw = settings
        .get(SINGLE_IMAGE_SECTION)
        .and_then(Value::as_object)
        .and_then(|section| section.get(SINGLE_IMAGE_JPEG_QUALITY_KEY))
        .and_then(Value::as_i64);
    let Some(raw) = raw else {
        return JPEG_QUALITY_DEFAULT;
    };
    let clamped = raw.clamp(i64::from(JPEG_QUALITY_MIN), i64::from(JPEG_QUALITY_MAX));
    // `clamped` lies in 1..=100, so the conversion cannot fail; the fallback only keeps the
    // function total without an `expect`.
    u8::try_from(clamped).unwrap_or(JPEG_QUALITY_DEFAULT)
}

/// Persists `quality` (clamped into `JPEG_QUALITY_MIN..=JPEG_QUALITY_MAX`) as
/// `SingleImage.jpeg_quality` in the user-config document at `user_settings_file` (production
/// callers pass `crate::user_config_path()`). One serialized `ms_docstore::update` that edits
/// only this key; every other key survives. Blocking disk I/O: never call on the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then left
/// untouched) or cannot be written.
pub fn save_jpeg_quality(user_settings_file: &Path, quality: u8) -> Result<(), String> {
    let quality = quality.clamp(JPEG_QUALITY_MIN, JPEG_QUALITY_MAX);
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, SINGLE_IMAGE_SECTION, |section_obj| {
            section_obj.insert(SINGLE_IMAGE_JPEG_QUALITY_KEY.to_owned(), Value::Number(quality.into()));
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn jpeg_quality_defaults_when_missing_or_not_an_integer() {
        assert_eq!(jpeg_quality_from(&json!({})), JPEG_QUALITY_DEFAULT);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": 7})), JPEG_QUALITY_DEFAULT);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {}})), JPEG_QUALITY_DEFAULT);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": "80"}})), JPEG_QUALITY_DEFAULT);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": 80.5}})), JPEG_QUALITY_DEFAULT);
    }

    #[test]
    fn jpeg_quality_clamps_into_range() {
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": 80}})), 80);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": 0}})), JPEG_QUALITY_MIN);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": -5}})), JPEG_QUALITY_MIN);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": 101}})), JPEG_QUALITY_MAX);
        assert_eq!(jpeg_quality_from(&json!({"SingleImage": {"jpeg_quality": u64::MAX}})), JPEG_QUALITY_DEFAULT);
    }

    #[test]
    fn defaults_tree_carries_the_default_quality() {
        assert_eq!(jpeg_quality_from(&crate::user_config_defaults()), JPEG_QUALITY_DEFAULT);
    }

    #[test]
    fn input_table_is_lowercase_dotless_and_unique() {
        assert!(!INPUT_FILE_TYPES.is_empty());
        let mut seen = HashSet::new();
        for ext in input_extensions() {
            assert!(!ext.is_empty());
            assert!(!ext.starts_with('.'), "{ext}");
            assert_eq!(ext, ext.to_ascii_lowercase(), "{ext}");
            assert!(seen.insert(ext), "duplicate extension {ext}");
        }
        let mut mimes = HashSet::new();
        for file_type in INPUT_FILE_TYPES {
            assert!(!file_type.extensions.is_empty());
            assert!(file_type.mime.starts_with("image/"), "{}", file_type.mime);
            assert!(mimes.insert(file_type.mime), "duplicate mime {}", file_type.mime);
        }
        assert!(seen.contains("jpe") && seen.contains("tiff") && seen.contains("qoi"));
        assert!(!seen.contains("avif"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scratch_base_is_a_per_user_directory_under_the_os_temp_dir() {
        let base = scratch_base();
        assert_eq!(base.parent(), Some(std::env::temp_dir().as_path()));
        let name = base.file_name().and_then(|name| name.to_str()).expect("UTF-8 scratch dir name");
        assert!(name.starts_with(SCRATCH_DIR_NAME), "{name}");
        #[cfg(unix)]
        {
            let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok();
            assert_eq!(name, scratch_dir_name(user.as_deref()));
        }
        #[cfg(not(unix))]
        assert_eq!(name, SCRATCH_DIR_NAME, "the Windows temp dir is already per-user");
    }

    /// Review Low-3: two accounts sharing `/tmp` get different scratch bases, and no user name can
    /// escape the temp directory or produce an odd component.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scratch_dir_name_is_per_user_and_sanitized() {
        assert_eq!(scratch_dir_name(Some("alice")), "manhwastudio-single-image-alice");
        assert_ne!(scratch_dir_name(Some("alice")), scratch_dir_name(Some("bob")));
        assert_eq!(scratch_dir_name(Some("a.b/c\\d")), "manhwastudio-single-image-a_b_c_d");
        assert_eq!(scratch_dir_name(Some("..")), "manhwastudio-single-image-__");
        assert_eq!(scratch_dir_name(Some("Пётр")), "manhwastudio-single-image-____");
        assert_eq!(scratch_dir_name(Some("")), SCRATCH_DIR_NAME);
        assert_eq!(scratch_dir_name(None), SCRATCH_DIR_NAME);
        let long = "x".repeat(100);
        assert_eq!(scratch_dir_name(Some(&long)).len(), SCRATCH_DIR_NAME.len() + 1 + SCRATCH_USER_SUFFIX_MAX);
    }

    #[test]
    fn save_jpeg_quality_round_trips_and_preserves_unrelated_keys() {
        // Temp document only: never the real `user_config` (PROJECT_RULES test hygiene).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, r#"{"General":{"theme":"dark"},"SingleImage":{"other":1}}"#).expect("seed document");

        save_jpeg_quality(&path, 70).expect("save 70");
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("valid json");
        assert_eq!(jpeg_quality_from(&root), 70);
        assert_eq!(root["General"]["theme"], json!("dark"));
        assert_eq!(root["SingleImage"]["other"], json!(1));
        assert!(root["SingleImage"]["jpeg_quality"].is_u64());

        save_jpeg_quality(&path, 0).expect("save clamped");
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("valid json");
        assert_eq!(jpeg_quality_from(&root), JPEG_QUALITY_MIN);
    }

    #[test]
    fn save_jpeg_quality_leaves_a_malformed_document_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("user_config.json");
        let malformed = "{ not json";
        std::fs::write(&path, malformed).expect("seed document");

        assert!(save_jpeg_quality(&path, 50).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), malformed);
    }
}
