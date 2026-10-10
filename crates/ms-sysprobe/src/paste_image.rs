/*
File: crates/ms-sysprobe/src/paste_image.rs

Purpose:
Cross-platform clipboard image reader shared by launcher and editor tabs.

Main responsibilities:
- read one RGBA image from the system clipboard through `ms_platform::clipboard`, trying the
  image itself, then a copied file list, then an image path/URI in the text or HTML;
- provide a single normalized (length-validated) image contract for callers;
- add Linux fallbacks for common Wayland/X11 clipboard tools when the platform reader is
  insufficient.

Key structures:
- ClipboardImage

Key functions:
- read_image_from_clipboard()

Dependencies:
- `ms_platform::clipboard` for every system clipboard read (it decides per platform what is
  readable: everything on desktop, text only on Android, nothing on wasm);
- `image` for decoding referenced image files and Linux helper output;
- `wl-paste`, `xclip`, `xsel` as optional runtime helpers on Linux.

Notes:
It lives in this crate because it is a host-environment probe like `screen_capture`, and
because its callers now span crates (the typing tab, the translation tab, the launcher);
the binary names it directly as `ms_sysprobe::paste_image`.
This module returns RGBA pixels only and blocks (clipboard transfer, file decode, helper
processes): callers remain responsible for running it on a worker thread and converting the
result into egui or image crate types.
A failed alternative representation is logged (never silently dropped) and the chain moves
on; only the primary image-read error (plus the Linux fallback error) reaches the caller.
*/

use ms_log::runtime_log;
use ms_platform::clipboard::{self, ClipboardError};
#[cfg(target_os = "linux")]
use std::env;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxClipboardSession {
    Wayland,
    X11,
    Unknown,
}

#[cfg(target_os = "linux")]
type LinuxClipboardReader = fn() -> Result<Option<ClipboardImage>, String>;

/// A clipboard image whose `rgba` is exactly `width * height * 4` bytes (rows top to bottom).
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Reads an RGBA image from the system clipboard. Blocking: call it from a worker.
///
/// Tries the clipboard image, then the alternative representations (file list, an image
/// path or `file://` URI in the text, then in the HTML), then — on Linux — the command-line
/// helpers. A platform without a reader for some representation (Android: no image, file
/// list or HTML; wasm: nothing) answers that step with a typed `Unavailable`, which is logged
/// and skipped like any other failed alternative.
///
/// # Errors
/// The localized message of the primary image-read failure; on Linux combined with the
/// helper fallback's error.
pub fn read_image_from_clipboard() -> Result<ClipboardImage, String> {
    match read_image_from_platform_clipboard() {
        Ok(image) => Ok(image),
        Err(primary_error) => {
            match read_image_from_clipboard_alternatives() {
                Ok(image) => return Ok(image),
                Err(alternatives_error) => {
                    runtime_log::log_info(format!("[paste-image] no image in the alternative clipboard representations: {alternatives_error}"));
                }
            }
            #[cfg(target_os = "linux")]
            {
                match read_image_from_clipboard_linux_fallback() {
                    Ok(image) => Ok(image),
                    Err(fallback_error) => Err(tf!("paste_image.linux_fallback_failed", primary_error = primary_error, fallback_error = fallback_error)),
                }
            }
            #[cfg(not(target_os = "linux"))]
            Err(primary_error)
        }
    }
}

fn read_image_from_platform_clipboard() -> Result<ClipboardImage, String> {
    let image = clipboard::read_image().map_err(|err| err.user_message())?;
    validate_rgba_image(image.width, image.height, image.rgba)
}

/// Looks for an image file referenced by the non-image clipboard representations. Read
/// failures of a representation are logged and the next one is tried; a referenced file that
/// exists but cannot be decoded ends the search with that error (the user clearly copied it).
fn read_image_from_clipboard_alternatives() -> Result<ClipboardImage, String> {
    match clipboard::read_file_list() {
        Ok(paths) => {
            if let Some(image) = paths.into_iter().find_map(|path| read_image_from_path(path.as_path()).ok()) {
                return Ok(image);
            }
        }
        Err(err) => log_alternative_read_failure("file list", &err),
    }

    match clipboard::read_text() {
        Ok(Some(text)) => {
            if let Some(image) = try_decode_image_reference(text.as_str())? {
                return Ok(image);
            }
        }
        Ok(None) => {}
        Err(err) => log_alternative_read_failure("text", &err),
    }

    match clipboard::read_html() {
        Ok(Some(html)) => {
            if let Some(image) = try_decode_image_reference(html.as_str())? {
                return Ok(image);
            }
        }
        Ok(None) => {}
        Err(err) => log_alternative_read_failure("html", &err),
    }

    Err(t!("paste_image.no_image_path").to_string())
}

/// Logs a failed alternative read. `NoContent` is the normal "not this kind" answer and is
/// not logged; an unavailable reader is expected on mobile/web (info), a backend failure is a
/// real problem (warn).
fn log_alternative_read_failure(representation: &str, err: &ClipboardError) {
    match err {
        ClipboardError::NoContent => {}
        ClipboardError::Unavailable(unavailable) => {
            runtime_log::log_info(format!("[paste-image] clipboard {representation} reader unavailable: {unavailable:?}"));
        }
        ClipboardError::Backend { detail } => {
            runtime_log::log_warn(format!("[paste-image] clipboard {representation} read failed: {detail}"));
        }
    }
}

fn validate_rgba_image(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
) -> Result<ClipboardImage, String> {
    if width == 0 || height == 0 {
        return Err(t!("paste_image.zero_size").to_string());
    }
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| t!("paste_image.too_large").to_string())?;
    if rgba.len() != expected_len {
        return Err(tf!("paste_image.unsupported_format", expected_len = expected_len, rgba = rgba.len()));
    }
    Ok(ClipboardImage {
        width,
        height,
        rgba,
    })
}

fn read_image_from_path(path: &Path) -> Result<ClipboardImage, String> {
    let decoded = image::open(path)
        .map_err(|err| tf!("paste_image.open_error", path = path.display(), err = err))?
        .to_rgba8();
    validate_rgba_image(
        usize::try_from(decoded.width()).unwrap_or(usize::MAX),
        usize::try_from(decoded.height()).unwrap_or(usize::MAX),
        decoded.into_raw(),
    )
}

fn try_decode_image_reference(raw: &str) -> Result<Option<ClipboardImage>, String> {
    let Some(path) = extract_image_path_reference(raw) else {
        return Ok(None);
    };
    read_image_from_path(path.as_path()).map(Some)
}

fn extract_image_path_reference(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    for candidate in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(path) = parse_file_uri(candidate) {
            return Some(path);
        }
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }

    None
}

fn parse_file_uri(raw: &str) -> Option<PathBuf> {
    let uri = raw.strip_prefix("file://")?;
    let decoded = decode_percent_escaped(uri)?;
    let path = PathBuf::from(decoded);
    path.is_file().then_some(path)
}

fn decode_percent_escaped(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let value = hex_to_u8(high, low)?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_to_u8(high: u8, low: u8) -> Option<u8> {
    Some((hex_nibble(high)? << 4) | hex_nibble(low)?)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn read_image_from_clipboard_linux_fallback() -> Result<ClipboardImage, String> {
    let session = detect_linux_clipboard_session();
    let mut errors = Vec::new();

    let commands: &[LinuxClipboardReader] = match session {
        LinuxClipboardSession::Wayland => &[
            read_image_with_wl_paste,
            read_image_with_xclip,
            read_image_with_xsel,
        ],
        LinuxClipboardSession::X11 => &[
            read_image_with_xclip,
            read_image_with_xsel,
            read_image_with_wl_paste,
        ],
        LinuxClipboardSession::Unknown => &[
            read_image_with_wl_paste,
            read_image_with_xclip,
            read_image_with_xsel,
        ],
    };

    for read_command in commands {
        match read_command() {
            Ok(Some(image)) => return Ok(image),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }

    if errors.is_empty() {
        Err(tf!("paste_image.linux_helpers_failed", session = session.as_str()))
    } else {
        Err(format!(
            "session: {}; {}",
            session.as_str(),
            errors.join(" | ")
        ))
    }
}

#[cfg(target_os = "linux")]
impl LinuxClipboardSession {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_linux_clipboard_session() -> LinuxClipboardSession {
    match env::var("XDG_SESSION_TYPE") {
        Ok(session_type) if session_type.eq_ignore_ascii_case("wayland") => {
            return LinuxClipboardSession::Wayland;
        }
        Ok(session_type) if session_type.eq_ignore_ascii_case("x11") => {
            return LinuxClipboardSession::X11;
        }
        Ok(_) | Err(env::VarError::NotPresent) => {}
        Err(env::VarError::NotUnicode(_)) => {}
    }

    if has_non_empty_env_var("WAYLAND_DISPLAY") {
        return LinuxClipboardSession::Wayland;
    }
    if has_non_empty_env_var("DISPLAY") {
        return LinuxClipboardSession::X11;
    }
    LinuxClipboardSession::Unknown
}

#[cfg(target_os = "linux")]
fn has_non_empty_env_var(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn decode_image_from_memory(bytes: &[u8]) -> Result<ClipboardImage, String> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|err| tf!("paste_image.decode_error", err = err))?
        .to_rgba8();
    validate_rgba_image(
        usize::try_from(decoded.width()).unwrap_or(usize::MAX),
        usize::try_from(decoded.height()).unwrap_or(usize::MAX),
        decoded.into_raw(),
    )
}

#[cfg(target_os = "linux")]
const PREFERRED_IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/bmp",
    "image/webp",
    "image/tiff",
    "image/gif",
];

#[cfg(target_os = "linux")]
fn read_image_with_wl_paste() -> Result<Option<ClipboardImage>, String> {
    let mime_types = query_wl_paste_mime_types()?;
    read_image_with_command_candidates("wl-paste", mime_types.as_slice(), |mime_type| {
        vec!["--no-newline", "--type", mime_type]
    })
}

#[cfg(target_os = "linux")]
fn read_image_with_xclip() -> Result<Option<ClipboardImage>, String> {
    let mut mime_types = query_xclip_mime_types()?;
    if mime_types.is_empty() {
        mime_types = PREFERRED_IMAGE_MIME_TYPES
            .iter()
            .map(|mime_type| (*mime_type).to_string())
            .collect();
    }
    read_image_with_command_candidates("xclip", mime_types.as_slice(), |mime_type| {
        vec!["-selection", "clipboard", "-t", mime_type, "-o"]
    })
}

#[cfg(target_os = "linux")]
fn read_image_with_xsel() -> Result<Option<ClipboardImage>, String> {
    let mime_types = query_xsel_mime_types()?;
    read_image_with_command_candidates("xsel", mime_types.as_slice(), |mime_type| {
        vec!["--clipboard", "--output", "--mime-type", mime_type]
    })
}

#[cfg(target_os = "linux")]
fn read_image_with_command_candidates<F>(
    binary: &str,
    mime_types: &[String],
    build_args: F,
) -> Result<Option<ClipboardImage>, String>
where
    F: Fn(&str) -> Vec<&str>,
{
    if mime_types.is_empty() {
        return Ok(None);
    }

    let mut last_error: Option<String> = None;
    for mime_type in mime_types {
        let args = build_args(mime_type);
        match Command::new(binary).args(args).output() {
            Ok(output) => {
                if !output.status.success() || output.stdout.is_empty() {
                    continue;
                }
                match decode_image_from_memory(output.stdout.as_slice()) {
                    Ok(image) => return Ok(Some(image)),
                    Err(error) => last_error = Some(format!("{binary} {mime_type}: {error}")),
                }
            }
            Err(error) => return Err(tf!("paste_image.binary_unavailable", binary = binary, error = error)),
        }
    }

    if let Some(error) = last_error {
        Err(error)
    } else {
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn query_wl_paste_mime_types() -> Result<Vec<String>, String> {
    let output = Command::new("wl-paste")
        .args(["--list-types"])
        .output()
        .map_err(|err| tf!("paste_image.wl_paste_unavailable", err = err))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_preferred_image_mime_types(output.stdout.as_slice()))
}

#[cfg(target_os = "linux")]
fn query_xclip_mime_types() -> Result<Vec<String>, String> {
    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output()
        .map_err(|err| tf!("paste_image.xclip_unavailable", err = err))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_preferred_image_mime_types(output.stdout.as_slice()))
}

#[cfg(target_os = "linux")]
fn query_xsel_mime_types() -> Result<Vec<String>, String> {
    let output = Command::new("xsel")
        .args(["--clipboard", "--output", "--mime-type", "TARGETS"])
        .output()
        .map_err(|err| tf!("paste_image.xsel_unavailable", err = err))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_preferred_image_mime_types(output.stdout.as_slice()))
}

#[cfg(target_os = "linux")]
fn parse_preferred_image_mime_types(raw: &[u8]) -> Vec<String> {
    let discovered_types: Vec<String> = String::from_utf8_lossy(raw)
        .lines()
        .map(str::trim)
        .filter(|mime_type| !mime_type.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    let mut preferred = Vec::new();
    for preferred_type in PREFERRED_IMAGE_MIME_TYPES {
        if discovered_types
            .iter()
            .any(|mime_type| mime_type == preferred_type)
        {
            preferred.push((*preferred_type).to_string());
        }
    }
    for mime_type in discovered_types {
        if mime_type.starts_with("image/") && !preferred.iter().any(|entry| entry == &mime_type) {
            preferred.push(mime_type);
        }
    }
    preferred
}

#[cfg(test)]
mod tests {
    use super::{decode_percent_escaped, extract_image_path_reference, try_decode_image_reference, validate_rgba_image};

    #[test]
    fn validate_rgba_image_checks_size_and_length() {
        assert!(validate_rgba_image(0, 1, Vec::new()).is_err());
        assert!(validate_rgba_image(2, 1, vec![0; 7]).is_err());
        assert!(validate_rgba_image(usize::MAX, 2, Vec::new()).is_err());
        let image = validate_rgba_image(2, 1, vec![7; 8]).unwrap_or_else(|err| panic!("valid 2x1 image rejected: {err}"));
        assert_eq!((image.width, image.height, image.rgba.len()), (2, 1, 8));
    }

    #[test]
    fn percent_decoding_handles_escapes_and_rejects_truncation() {
        assert_eq!(decode_percent_escaped("/tmp/a%20b%2Fc").as_deref(), Some("/tmp/a b/c"));
        assert_eq!(decode_percent_escaped("%4"), None);
        assert_eq!(decode_percent_escaped("%zz"), None);
    }

    #[test]
    fn image_reference_is_found_in_text_lines_and_file_uris() {
        let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("temp dir: {err}"));
        let path = dir.path().join("pasted image.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255])).save(&path).unwrap_or_else(|err| panic!("write png: {err}"));

        let plain = format!("\n  {}\n", path.display());
        assert_eq!(extract_image_path_reference(&plain), Some(path.clone()));
        let uri = format!("file://{}", path.display().to_string().replace(' ', "%20"));
        assert_eq!(extract_image_path_reference(&uri), Some(path.clone()));
        assert_eq!(extract_image_path_reference("not a path"), None);
        assert_eq!(extract_image_path_reference("   "), None);

        let decoded = try_decode_image_reference(&plain).unwrap_or_else(|err| panic!("decode: {err}")).unwrap_or_else(|| panic!("no image decoded"));
        assert_eq!((decoded.width, decoded.height, decoded.rgba.len()), (3, 2, 24));
    }

    #[test]
    fn referenced_file_that_is_not_an_image_is_an_error() {
        let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("temp dir: {err}"));
        let path = dir.path().join("note.txt");
        std::fs::write(&path, b"plain text").unwrap_or_else(|err| panic!("write: {err}"));
        assert!(try_decode_image_reference(&path.display().to_string()).is_err());
    }
}
