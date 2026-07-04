/*
File: src/paste_image.rs

Purpose:
Cross-platform clipboard image reader shared by launcher and editor tabs.

Main responsibilities:
- read RGBA images from the system clipboard without blocking the GUI thread;
- provide a single normalized image contract for callers;
- add Linux fallbacks for common Wayland/X11 clipboard tools when `arboard` is insufficient.

Key structures:
- ClipboardImage

Key functions:
- read_image_from_clipboard()

Dependencies:
- `arboard` for primary clipboard access;
- `image` for decoding fallback clipboard payloads on Linux;
- `wl-paste`, `xclip`, `xsel` as optional runtime helpers on Linux.

Notes:
This module returns RGBA pixels only. Callers remain responsible for spawning a worker thread
and converting the result into egui or image crate types.
*/

// Native-only clipboard backend; absent on the wasm target.
#[cfg(not(target_arch = "wasm32"))]
use arboard::Clipboard;
#[cfg(target_os = "linux")]
use std::env;
// Path helpers are only used by the native clipboard readers below.
#[cfg(not(target_arch = "wasm32"))]
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

#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Reads an RGBA image from the system clipboard.
///
/// Native builds try `arboard`, then alternative clipboard representations, then
/// Linux command-line helpers. On wasm there is no system clipboard image API, so
/// this returns a clear error instead of a fake success.
#[cfg(target_arch = "wasm32")]
pub fn read_image_from_clipboard() -> Result<ClipboardImage, String> {
    Err("буфер обмена изображений недоступен в веб-версии".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_image_from_clipboard() -> Result<ClipboardImage, String> {
    match read_image_from_clipboard_arboard() {
        Ok(image) => Ok(image),
        Err(primary_error) => {
            if let Ok(image) = read_image_from_clipboard_arboard_alternatives() {
                return Ok(image);
            }
            #[cfg(target_os = "linux")]
            {
                match read_image_from_clipboard_linux_fallback() {
                    Ok(image) => Ok(image),
                    Err(fallback_error) => Err(format!(
                        "{primary_error}. Linux fallback тоже не сработал: {fallback_error}"
                    )),
                }
            }
            #[cfg(not(target_os = "linux"))]
            Err(primary_error)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_image_from_clipboard_arboard() -> Result<ClipboardImage, String> {
    let mut clipboard = Clipboard::new().map_err(|err| err.to_string())?;
    let image = clipboard.get_image().map_err(|err| err.to_string())?;
    validate_rgba_image(image.width, image.height, image.bytes.into_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_image_from_clipboard_arboard_alternatives() -> Result<ClipboardImage, String> {
    let mut clipboard = Clipboard::new().map_err(|err| err.to_string())?;

    if let Ok(paths) = clipboard.get().file_list()
        && let Some(image) = paths
            .into_iter()
            .find_map(|path| read_image_from_path(path.as_path()).ok())
    {
        return Ok(image);
    }

    if let Ok(text) = clipboard.get_text()
        && let Some(image) = try_decode_image_reference(text.as_str())?
    {
        return Ok(image);
    }

    if let Ok(html) = clipboard.get().html()
        && let Some(image) = try_decode_image_reference(html.as_str())?
    {
        return Ok(image);
    }

    Err("альтернативные представления буфера не содержат путь к изображению".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_rgba_image(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
) -> Result<ClipboardImage, String> {
    if width == 0 || height == 0 {
        return Err("в буфере изображение нулевого размера".to_string());
    }
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| "слишком большой размер изображения в буфере".to_string())?;
    if rgba.len() != expected_len {
        return Err(format!(
            "неподдерживаемый формат буфера: ожидалось {expected_len} байт RGBA, получено {}",
            rgba.len()
        ));
    }
    Ok(ClipboardImage {
        width,
        height,
        rgba,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn read_image_from_path(path: &Path) -> Result<ClipboardImage, String> {
    let decoded = image::open(path)
        .map_err(|err| format!("не удалось открыть изображение '{}': {err}", path.display()))?
        .to_rgba8();
    validate_rgba_image(
        usize::try_from(decoded.width()).unwrap_or(usize::MAX),
        usize::try_from(decoded.height()).unwrap_or(usize::MAX),
        decoded.into_raw(),
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn try_decode_image_reference(raw: &str) -> Result<Option<ClipboardImage>, String> {
    let Some(path) = extract_image_path_reference(raw) else {
        return Ok(None);
    };
    read_image_from_path(path.as_path()).map(Some)
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn parse_file_uri(raw: &str) -> Option<PathBuf> {
    let uri = raw.strip_prefix("file://")?;
    let decoded = decode_percent_escaped(uri)?;
    let path = PathBuf::from(decoded);
    path.is_file().then_some(path)
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn hex_to_u8(high: u8, low: u8) -> Option<u8> {
    Some((hex_nibble(high)? << 4) | hex_nibble(low)?)
}

#[cfg(not(target_arch = "wasm32"))]
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
        Err(format!(
            "не удалось получить изображение из буфера через Linux clipboard helpers (session: {})",
            session.as_str()
        ))
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
        .map_err(|err| format!("не удалось декодировать изображение из буфера: {err}"))?
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
            Err(error) => return Err(format!("{binary} недоступен: {error}")),
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
        .map_err(|err| format!("wl-paste недоступен: {err}"))?;
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
        .map_err(|err| format!("xclip недоступен: {err}"))?;
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
        .map_err(|err| format!("xsel недоступен: {err}"))?;
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
