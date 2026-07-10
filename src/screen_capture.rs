/*
File: src/screen_capture.rs

Purpose:
Cross-platform desktop region capture helpers shared by launcher capture flows.

Main responsibilities:
- query the virtual desktop bounds used by overlay-based capture UIs;
- capture the current screen contents of a selected rectangle without blocking the GUI thread;
- hide platform-specific command/API differences behind a single RGBA image contract.

Key structures:
- ScreenRect

Key functions:
- query_virtual_desktop_bounds()
- capture_screen_rect()

Dependencies:
- `image` for decoding command output and returning `RgbaImage`;
- Windows GDI on Windows;
- optional Linux runtime helpers such as `grim`, `maim`, `import`, `xrandr`, `hyprctl`;
- the built-in `/usr/sbin/screencapture` CLI tool on macOS.

Notes:
This module performs blocking OS calls and must always run on a worker thread.
*/

use image::RgbaImage;

#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "windows")]
use std::mem::{size_of, zeroed};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ,
    ReleaseDC, SRCCOPY, SelectObject,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRect {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[cfg(target_os = "linux")]
type LinuxCaptureCommand = fn(ScreenRect) -> Result<RgbaImage, String>;

pub fn query_virtual_desktop_bounds() -> Result<ScreenRect, String> {
    #[cfg(target_os = "windows")]
    {
        query_virtual_desktop_bounds_windows()
    }
    #[cfg(target_os = "linux")]
    {
        query_virtual_desktop_bounds_linux()
    }
    #[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
    {
        query_virtual_desktop_bounds_macos()
    }
    // Web build: no desktop to query — the capture flow is a native launcher feature.
    #[cfg(target_arch = "wasm32")]
    {
        Err(t!("screen_capture.web_unavailable").to_string())
    }
    #[cfg(all(
        not(target_arch = "wasm32"),
        not(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos"
        ))
    ))]
    {
        Err("desktop capture is not supported on this platform".to_string())
    }
}

pub fn capture_screen_rect(rect: ScreenRect) -> Result<RgbaImage, String> {
    if rect.is_empty() {
        return Err("screen capture region is empty".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        capture_screen_rect_windows(rect)
    }
    #[cfg(target_os = "linux")]
    {
        capture_screen_rect_linux(rect)
    }
    #[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
    {
        capture_screen_rect_macos(rect)
    }
    // Web build: no desktop to capture — the capture flow is a native launcher feature.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = rect;
        Err(t!("screen_capture.web_unavailable").to_string())
    }
    #[cfg(all(
        not(target_arch = "wasm32"),
        not(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos"
        ))
    ))]
    {
        let _ = rect;
        Err("desktop capture is not supported on this platform".to_string())
    }
}

#[cfg(target_os = "windows")]
fn query_virtual_desktop_bounds_windows() -> Result<ScreenRect, String> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Err("Windows returned an empty virtual desktop".to_string());
    }
    Ok(ScreenRect {
        x,
        y,
        width: u32::try_from(width).map_err(|_| "virtual desktop width overflow".to_string())?,
        height: u32::try_from(height).map_err(|_| "virtual desktop height overflow".to_string())?,
    })
}

#[cfg(target_os = "windows")]
fn capture_screen_rect_windows(rect: ScreenRect) -> Result<RgbaImage, String> {
    let width_i32 = i32::try_from(rect.width)
        .map_err(|_| "capture width is too large for Windows".to_string())?;
    let height_i32 = i32::try_from(rect.height)
        .map_err(|_| "capture height is too large for Windows".to_string())?;

    let screen_dc = unsafe { GetDC(HWND::default()) };
    if screen_dc.is_null() {
        return Err("GetDC failed for the virtual desktop".to_string());
    }

    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc.is_null() {
        unsafe {
            ReleaseDC(HWND::default(), screen_dc);
        }
        return Err("CreateCompatibleDC failed".to_string());
    }

    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width_i32, height_i32) };
    if bitmap.is_null() {
        unsafe {
            DeleteDC(memory_dc);
            ReleaseDC(HWND::default(), screen_dc);
        }
        return Err("CreateCompatibleBitmap failed".to_string());
    }

    let previous_object = unsafe { SelectObject(memory_dc, bitmap as HGDIOBJ) };
    if previous_object.is_null() {
        unsafe {
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(memory_dc);
            ReleaseDC(HWND::default(), screen_dc);
        }
        return Err("SelectObject failed for capture bitmap".to_string());
    }

    let blit_result = unsafe {
        BitBlt(
            memory_dc,
            0,
            0,
            width_i32,
            height_i32,
            screen_dc,
            rect.x,
            rect.y,
            SRCCOPY | CAPTUREBLT,
        )
    };
    if blit_result == 0 {
        unsafe {
            SelectObject(memory_dc, previous_object);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(memory_dc);
            ReleaseDC(HWND::default(), screen_dc);
        }
        return Err("BitBlt failed while capturing the screen".to_string());
    }

    let pixel_len = usize::try_from(rect.width)
        .ok()
        .and_then(|width| {
            usize::try_from(rect.height)
                .ok()
                .map(|height| width * height)
        })
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| "capture buffer is too large".to_string())?;
    let mut bgra = vec![0u8; pixel_len];

    let mut bitmap_info: BITMAPINFO = unsafe { zeroed() };
    bitmap_info.bmiHeader = BITMAPINFOHEADER {
        biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())
            .map_err(|_| "BITMAPINFOHEADER size overflow".to_string())?,
        biWidth: width_i32,
        biHeight: -height_i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { zeroed() }
    };

    let scan_lines = unsafe {
        GetDIBits(
            memory_dc,
            bitmap,
            0,
            rect.height,
            bgra.as_mut_ptr().cast(),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        )
    };

    unsafe {
        SelectObject(memory_dc, previous_object);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(memory_dc);
        ReleaseDC(HWND::default(), screen_dc);
    }

    if scan_lines == 0 {
        return Err("GetDIBits failed for the captured bitmap".to_string());
    }

    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    RgbaImage::from_raw(rect.width, rect.height, bgra)
        .ok_or_else(|| "Windows capture returned an invalid RGBA buffer".to_string())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxSession {
    Wayland,
    X11,
    Unknown,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxDesktopEnvironment {
    KdePlasma,
    Other,
}

#[cfg(target_os = "linux")]
fn detect_linux_session() -> LinuxSession {
    match env::var("XDG_SESSION_TYPE") {
        Ok(session) if session.eq_ignore_ascii_case("wayland") => LinuxSession::Wayland,
        Ok(session) if session.eq_ignore_ascii_case("x11") => LinuxSession::X11,
        Ok(_) | Err(env::VarError::NotPresent) | Err(env::VarError::NotUnicode(_)) => {
            if has_non_empty_env_var("WAYLAND_DISPLAY") {
                LinuxSession::Wayland
            } else if has_non_empty_env_var("DISPLAY") {
                LinuxSession::X11
            } else {
                LinuxSession::Unknown
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn has_non_empty_env_var(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn detect_linux_desktop_environment() -> LinuxDesktopEnvironment {
    let desktop = env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| env::var("DESKTOP_SESSION"))
        .unwrap_or_default();
    if desktop.to_ascii_lowercase().contains("kde")
        || desktop.to_ascii_lowercase().contains("plasma")
    {
        LinuxDesktopEnvironment::KdePlasma
    } else {
        LinuxDesktopEnvironment::Other
    }
}

#[cfg(target_os = "linux")]
fn query_virtual_desktop_bounds_linux() -> Result<ScreenRect, String> {
    let session = detect_linux_session();
    let desktop_environment = detect_linux_desktop_environment();
    let bounds = match session {
        LinuxSession::Wayland if desktop_environment == LinuxDesktopEnvironment::KdePlasma => {
            query_plasma_wayland_desktop_bounds().or_else(|plasma_error| {
                query_wayland_desktop_bounds()
                    .map_err(|wayland_error| format!("{plasma_error} | {wayland_error}"))
            })?
        }
        LinuxSession::Wayland => query_wayland_desktop_bounds().or_else(|wayland_error| {
            query_xrandr_monitor_bounds()
                .map_err(|xrandr_error| format!("{wayland_error} | {xrandr_error}"))
        })?,
        LinuxSession::X11 => query_xrandr_monitor_bounds().or_else(|xrandr_error| {
            query_wayland_desktop_bounds()
                .map_err(|wayland_error| format!("{xrandr_error} | {wayland_error}"))
        })?,
        LinuxSession::Unknown => query_wayland_desktop_bounds().or_else(|wayland_error| {
            query_xrandr_monitor_bounds()
                .map_err(|xrandr_error| format!("{wayland_error} | {xrandr_error}"))
        })?,
    };
    if bounds.is_empty() {
        return Err("Linux desktop bounds are empty".to_string());
    }
    Ok(bounds)
}

#[cfg(target_os = "linux")]
fn query_plasma_wayland_desktop_bounds() -> Result<ScreenRect, String> {
    let output = Command::new("kscreen-doctor")
        .args(["-j"])
        .output()
        .map_err(|err| format!("kscreen-doctor unavailable: {err}"))?;
    if !output.status.success() {
        return Err("kscreen-doctor did not return output geometry".to_string());
    }
    let json: Value = serde_json::from_slice(output.stdout.as_slice())
        .map_err(|err| format!("failed to parse kscreen-doctor json: {err}"))?;
    let outputs = json
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| "kscreen-doctor payload does not contain outputs".to_string())?;
    collect_monitor_union(outputs.iter().filter_map(|output| {
        let enabled = output
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return None;
        }
        let geometry = output.get("geometry")?;
        let x = geometry.get("x")?.as_i64()?;
        let y = geometry.get("y")?.as_i64()?;
        let width = geometry.get("width")?.as_u64()?;
        let height = geometry.get("height")?.as_u64()?;
        build_screen_rect(x, y, width, height)
    }))
    .ok_or_else(|| "kscreen-doctor returned no usable output geometry".to_string())
}

#[cfg(target_os = "linux")]
fn query_wayland_desktop_bounds() -> Result<ScreenRect, String> {
    query_hyprctl_monitor_bounds().or_else(|hypr_error| {
        query_wlr_randr_monitor_bounds().map_err(|wlr_error| format!("{hypr_error} | {wlr_error}"))
    })
}

#[cfg(target_os = "linux")]
fn query_hyprctl_monitor_bounds() -> Result<ScreenRect, String> {
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .map_err(|err| format!("hyprctl unavailable: {err}"))?;
    if !output.status.success() {
        return Err("hyprctl did not return monitor data".to_string());
    }
    let json: Value = serde_json::from_slice(output.stdout.as_slice())
        .map_err(|err| format!("failed to parse hyprctl monitor json: {err}"))?;
    let Some(monitors) = json.as_array() else {
        return Err("hyprctl monitor payload is not an array".to_string());
    };
    collect_monitor_union(monitors.iter().filter_map(|monitor| {
        let x = monitor.get("x")?.as_i64()?;
        let y = monitor.get("y")?.as_i64()?;
        let width = monitor.get("width")?.as_u64()?;
        let height = monitor.get("height")?.as_u64()?;
        build_screen_rect(x, y, width, height)
    }))
    .ok_or_else(|| "hyprctl returned no usable monitor geometry".to_string())
}

#[cfg(target_os = "linux")]
fn query_wlr_randr_monitor_bounds() -> Result<ScreenRect, String> {
    let output = Command::new("wlr-randr")
        .args(["--json"])
        .output()
        .map_err(|err| format!("wlr-randr unavailable: {err}"))?;
    if !output.status.success() {
        return Err("wlr-randr did not return monitor data".to_string());
    }
    let json: Value = serde_json::from_slice(output.stdout.as_slice())
        .map_err(|err| format!("failed to parse wlr-randr json: {err}"))?;
    let Some(monitors) = json.as_array() else {
        return Err("wlr-randr payload is not an array".to_string());
    };
    collect_monitor_union(monitors.iter().filter_map(|monitor| {
        let enabled = monitor
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return None;
        }
        let x = monitor.get("x").and_then(Value::as_i64)?;
        let y = monitor.get("y").and_then(Value::as_i64)?;
        let width = monitor
            .get("current_mode")
            .and_then(|mode| mode.get("width"))
            .and_then(Value::as_u64)
            .or_else(|| monitor.get("width").and_then(Value::as_u64))?;
        let height = monitor
            .get("current_mode")
            .and_then(|mode| mode.get("height"))
            .and_then(Value::as_u64)
            .or_else(|| monitor.get("height").and_then(Value::as_u64))?;
        build_screen_rect(x, y, width, height)
    }))
    .ok_or_else(|| "wlr-randr returned no usable monitor geometry".to_string())
}

#[cfg(target_os = "linux")]
fn query_xrandr_monitor_bounds() -> Result<ScreenRect, String> {
    let output = Command::new("xrandr")
        .args(["--listmonitors"])
        .output()
        .map_err(|err| format!("xrandr unavailable: {err}"))?;
    if !output.status.success() {
        return Err("xrandr did not return monitor data".to_string());
    }

    let monitors = String::from_utf8_lossy(output.stdout.as_slice());
    let mut rects = Vec::new();
    for line in monitors.lines().skip(1) {
        let Some(geometry) = line.split_whitespace().find(|part| part.contains('+')) else {
            continue;
        };
        if let Some(rect) = parse_xrandr_geometry(geometry) {
            rects.push(rect);
        }
    }
    collect_monitor_union(rects)
        .ok_or_else(|| "xrandr returned no usable monitor geometry".to_string())
}

#[cfg(target_os = "linux")]
fn parse_xrandr_geometry(raw: &str) -> Option<ScreenRect> {
    let mut parts = raw.split('+');
    let size = parts.next()?;
    let x = parts.next()?.parse::<i64>().ok()?;
    let y = parts.next()?.parse::<i64>().ok()?;
    let mut size_parts = size.split('/');
    let width = size_parts.next()?.parse::<u64>().ok()?;
    let _mm_width = size_parts.next();
    let height = size_parts.next()?.parse::<u64>().ok()?;
    build_screen_rect(x, y, width, height)
}

#[cfg(target_os = "linux")]
fn build_screen_rect(x: i64, y: i64, width: u64, height: u64) -> Option<ScreenRect> {
    Some(ScreenRect {
        x: i32::try_from(x).ok()?,
        y: i32::try_from(y).ok()?,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

#[cfg(target_os = "linux")]
fn collect_monitor_union<I>(rects: I) -> Option<ScreenRect>
where
    I: IntoIterator<Item = ScreenRect>,
{
    let mut left = i64::MAX;
    let mut top = i64::MAX;
    let mut right = i64::MIN;
    let mut bottom = i64::MIN;
    let mut any = false;

    for rect in rects {
        if rect.is_empty() {
            continue;
        }
        any = true;
        left = left.min(i64::from(rect.x));
        top = top.min(i64::from(rect.y));
        right = right.max(i64::from(rect.x) + i64::from(rect.width));
        bottom = bottom.max(i64::from(rect.y) + i64::from(rect.height));
    }

    if !any {
        return None;
    }

    Some(ScreenRect {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: u32::try_from((right - left).max(0)).ok()?,
        height: u32::try_from((bottom - top).max(0)).ok()?,
    })
}

#[cfg(target_os = "linux")]
fn capture_screen_rect_linux(rect: ScreenRect) -> Result<RgbaImage, String> {
    let session = detect_linux_session();
    let desktop_environment = detect_linux_desktop_environment();
    let mut errors = Vec::new();

    let commands: &[LinuxCaptureCommand] = match (session, desktop_environment) {
        (LinuxSession::Wayland, LinuxDesktopEnvironment::KdePlasma) => &[
            capture_with_kwin_screenshot_area,
            capture_with_kwin_screenshot_workspace,
            capture_with_grim,
            capture_with_maim,
            capture_with_import,
        ],
        (LinuxSession::Wayland, _) => &[capture_with_grim, capture_with_maim, capture_with_import],
        (LinuxSession::X11, _) => &[capture_with_maim, capture_with_import, capture_with_grim],
        (LinuxSession::Unknown, _) => &[capture_with_grim, capture_with_maim, capture_with_import],
    };

    for command in commands {
        match command(rect) {
            Ok(image) => return Ok(image),
            Err(error) => errors.push(error),
        }
    }

    Err(format!(
        "failed to capture Linux screen region in {:?} session: {}",
        session,
        errors.join(" | ")
    ))
}

#[cfg(target_os = "linux")]
fn capture_with_kwin_screenshot_area(rect: ScreenRect) -> Result<RgbaImage, String> {
    let path = run_kwin_screenshot_string_method(
        "screenshotArea",
        &[
            rect.x.to_string(),
            rect.y.to_string(),
            rect.width.to_string(),
            rect.height.to_string(),
        ],
    )?;
    load_image_from_path(&path, "KWin screenshotArea")
}

#[cfg(target_os = "linux")]
fn capture_with_kwin_screenshot_workspace(rect: ScreenRect) -> Result<RgbaImage, String> {
    let path = run_kwin_screenshot_string_method("screenshotFullscreen", &[])?;
    let image = load_image_from_path(&path, "KWin screenshotFullscreen")?;
    let workspace_bounds = query_plasma_wayland_desktop_bounds()?;
    crop_workspace_capture(image, workspace_bounds, rect)
}

#[cfg(target_os = "linux")]
fn run_kwin_screenshot_string_method(
    method_name: &str,
    args: &[String],
) -> Result<PathBuf, String> {
    if let Ok(path) = run_qdbus_kwin_method(method_name, args) {
        return Ok(path);
    }
    run_gdbus_kwin_method(method_name, args)
}

#[cfg(target_os = "linux")]
fn run_qdbus_kwin_method(method_name: &str, args: &[String]) -> Result<PathBuf, String> {
    for binary in ["qdbus6", "qdbus-qt6", "qdbus"] {
        let mut command = Command::new(binary);
        command.arg("org.kde.KWin");
        command.arg("/Screenshot");
        command.arg(format!("org.kde.kwin.Screenshot.{method_name}"));
        for arg in args {
            command.arg(arg);
        }
        let output = match command.output() {
            Ok(output) => output,
            Err(_) => continue,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(output.stderr.as_slice());
            let error_text = stderr.trim();
            return Err(format!(
                "{binary} {method_name} failed: {}",
                if error_text.is_empty() {
                    "unknown qdbus error"
                } else {
                    error_text
                }
            ));
        }
        return parse_kwin_screenshot_path(output.stdout.as_slice(), binary, method_name);
    }
    Err("qdbus/qdbus6 is unavailable".to_string())
}

#[cfg(target_os = "linux")]
fn run_gdbus_kwin_method(method_name: &str, args: &[String]) -> Result<PathBuf, String> {
    let mut command = Command::new("gdbus");
    command.args([
        "call",
        "--session",
        "--dest",
        "org.kde.KWin",
        "--object-path",
        "/Screenshot",
        "--method",
    ]);
    command.arg(format!("org.kde.kwin.Screenshot.{method_name}"));
    for arg in args {
        command.arg(arg);
    }
    let output = command
        .output()
        .map_err(|err| format!("gdbus unavailable: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(output.stderr.as_slice());
        let error_text = stderr.trim();
        return Err(format!(
            "gdbus {method_name} failed: {}",
            if error_text.is_empty() {
                "unknown gdbus error"
            } else {
                error_text
            }
        ));
    }
    parse_kwin_screenshot_path(output.stdout.as_slice(), "gdbus", method_name)
}

#[cfg(target_os = "linux")]
fn parse_kwin_screenshot_path(
    stdout: &[u8],
    binary: &str,
    method_name: &str,
) -> Result<PathBuf, String> {
    let text = String::from_utf8_lossy(stdout);
    let Some(start) = text.find('\'') else {
        return Err(format!(
            "{binary} {method_name} did not return a screenshot path"
        ));
    };
    let remainder = &text[start + 1..];
    let Some(end) = remainder.find('\'') else {
        return Err(format!(
            "{binary} {method_name} returned an invalid screenshot path"
        ));
    };
    let path = PathBuf::from(&remainder[..end]);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{binary} {method_name} returned a missing file '{}'",
            path.display()
        ))
    }
}

#[cfg(target_os = "linux")]
fn load_image_from_path(path: &Path, source: &str) -> Result<RgbaImage, String> {
    image::open(path)
        .map_err(|err| format!("{source} failed to open '{}': {err}", path.display()))
        .map(|image| image.to_rgba8())
}

#[cfg(target_os = "linux")]
fn crop_workspace_capture(
    image: RgbaImage,
    workspace_bounds: ScreenRect,
    target_rect: ScreenRect,
) -> Result<RgbaImage, String> {
    let offset_x = i64::from(target_rect.x) - i64::from(workspace_bounds.x);
    let offset_y = i64::from(target_rect.y) - i64::from(workspace_bounds.y);
    if offset_x < 0 || offset_y < 0 {
        return Err(
            "target capture region lies outside the Plasma workspace screenshot".to_string(),
        );
    }
    let crop_x = u32::try_from(offset_x).map_err(|_| "capture x offset overflow".to_string())?;
    let crop_y = u32::try_from(offset_y).map_err(|_| "capture y offset overflow".to_string())?;
    let image_width = image.width();
    let image_height = image.height();
    if crop_x.saturating_add(target_rect.width) > image_width
        || crop_y.saturating_add(target_rect.height) > image_height
    {
        return Err(
            "Plasma workspace screenshot is smaller than the requested capture region".to_string(),
        );
    }
    Ok(image::imageops::crop_imm(
        &image,
        crop_x,
        crop_y,
        target_rect.width,
        target_rect.height,
    )
    .to_image())
}

#[cfg(target_os = "linux")]
fn capture_with_grim(rect: ScreenRect) -> Result<RgbaImage, String> {
    let geometry =
        format!("{},{ } {}x{}", rect.x, rect.y, rect.width, rect.height).replace(", ", ",");
    let output = Command::new("grim")
        .args(["-g", geometry.as_str(), "-"])
        .output()
        .map_err(|err| format!("grim unavailable: {err}"))?;
    decode_capture_command_output("grim", output.status.success(), output.stdout.as_slice())
}

#[cfg(target_os = "linux")]
fn capture_with_maim(rect: ScreenRect) -> Result<RgbaImage, String> {
    let geometry = format!("{}x{}+{}+{}", rect.width, rect.height, rect.x, rect.y);
    let output = Command::new("maim")
        .args(["-g", geometry.as_str()])
        .output()
        .map_err(|err| format!("maim unavailable: {err}"))?;
    decode_capture_command_output("maim", output.status.success(), output.stdout.as_slice())
}

#[cfg(target_os = "linux")]
fn capture_with_import(rect: ScreenRect) -> Result<RgbaImage, String> {
    let geometry = format!("{}x{}+{}+{}", rect.width, rect.height, rect.x, rect.y);
    let output = Command::new("import")
        .args(["-window", "root", "-crop", geometry.as_str(), "png:-"])
        .output()
        .map_err(|err| format!("import unavailable: {err}"))?;
    decode_capture_command_output("import", output.status.success(), output.stdout.as_slice())
}

#[cfg(target_os = "linux")]
fn decode_capture_command_output(
    command_name: &str,
    success: bool,
    stdout: &[u8],
) -> Result<RgbaImage, String> {
    if !success {
        return Err(format!(
            "{command_name} failed to capture the requested region"
        ));
    }
    if stdout.is_empty() {
        return Err(format!("{command_name} returned empty image data"));
    }
    image::load_from_memory(stdout)
        .map_err(|err| format!("{command_name} returned invalid image data: {err}"))
        .map(|image| image.to_rgba8())
}

// --- macOS backend ---------------------------------------------------------
//
// macOS ships the `screencapture` CLI at a fixed system path. Mirroring the
// Linux CLI approach, both public entry points shell out to it on a worker
// thread. `screencapture -R` consumes a global rectangle expressed in *points*
// (origin at the top-left of the main display); the PNG it writes is in device
// pixels, so on HiDPI/Retina displays the decoded image is larger than the
// requested point rectangle. That is harmless: the import path makes no
// dimension assumption and simply gains a sharper (2x) image.
//
// `query_virtual_desktop_bounds_macos` reports the main display size in *device
// pixels* (from a PNG header), which does NOT match the point space the overlay
// and `-R` use. The launcher therefore prefers egui's point-based monitor size
// for the overlay bounds and only falls back to this helper when that size is
// unavailable; see `resolve_capture_desktop_bounds` in the new-project window.

/// Absolute path to the built-in macOS screenshot CLI. It is always present on
/// a stock macOS install; capture fails with a clear error if it is missing.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
const MACOS_SCREENCAPTURE_PATH: &str = "/usr/sbin/screencapture";

/// RAII guard that deletes a temporary screenshot PNG on drop so every early
/// return and error path cleans up. A `NotFound` removal is treated as success
/// (screencapture may never have created the file); any other failure is logged
/// via `runtime_log` at warn level instead of being silently ignored.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
struct MacosCaptureTempFile {
    path: PathBuf,
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
impl MacosCaptureTempFile {
    /// Takes ownership of `path`; the file it points to is removed when the
    /// guard is dropped. The file itself is created later by `screencapture`.
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Borrows the temporary file path for building command arguments.
    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
impl Drop for MacosCaptureTempFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            // The file was never written (e.g. screencapture failed): nothing to clean up.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "failed to remove temporary screencapture file '{}': {err}",
                    self.path.display()
                ));
            }
        }
    }
}

/// Verifies the `screencapture` tool exists at its fixed system path.
///
/// # Errors
/// Returns a user-facing error when the tool is missing, so capture never
/// silently falls back to a fake image.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn ensure_macos_screencapture_available() -> Result<(), String> {
    if Path::new(MACOS_SCREENCAPTURE_PATH).exists() {
        Ok(())
    } else {
        Err(format!(
            "macOS screen capture tool is missing at '{MACOS_SCREENCAPTURE_PATH}'"
        ))
    }
}

/// Formats the `screencapture -R` region argument from a `ScreenRect`.
///
/// The result is a single argument `-R<x>,<y>,<width>,<height>` where the values
/// are in points (screencapture's coordinate unit). This is a pure helper so the
/// argument formatting can be unit-tested without invoking the CLI.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn build_screencapture_region_arg(rect: ScreenRect) -> String {
    format!("-R{},{},{},{}", rect.x, rect.y, rect.width, rect.height)
}

/// Builds a process-unique temporary PNG path under the system temp directory.
///
/// Uniqueness comes from the process id plus a nanosecond timestamp, mirroring
/// the temp-path helpers used elsewhere in the codebase (no RNG dependency).
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn unique_macos_capture_path() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "manhwastudio_capture_{}_{nanos}.png",
        std::process::id()
    ))
}

/// Decodes a screencapture PNG file into an RGBA image.
///
/// `source` is a short description of the capture used only for error context.
///
/// # Errors
/// Returns a descriptive error if the file cannot be opened or decoded.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn load_macos_capture_image(path: &Path, source: &str) -> Result<RgbaImage, String> {
    image::open(path)
        .map_err(|err| {
            format!(
                "screencapture output for {source} could not be decoded from '{}': {err}",
                path.display()
            )
        })
        .map(|image| image.to_rgba8())
}

/// Captures a screen region on macOS via `/usr/sbin/screencapture`.
///
/// `rect` is a global rectangle in points. The capture is written to a unique
/// temporary PNG, decoded into an `RgbaImage`, and the temp file is removed by
/// the RAII guard on every return path (after decoding so the pixels are read
/// first).
///
/// # Errors
/// Returns a user-facing error if `screencapture` is missing, exits non-zero, or
/// its output cannot be decoded.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn capture_screen_rect_macos(rect: ScreenRect) -> Result<RgbaImage, String> {
    ensure_macos_screencapture_available()?;

    let temp_file = MacosCaptureTempFile::new(unique_macos_capture_path());
    let region_arg = build_screencapture_region_arg(rect);

    // `-x` suppresses the capture sound; `-R…` limits capture to the region;
    // the trailing path is the PNG screencapture writes.
    let output = Command::new(MACOS_SCREENCAPTURE_PATH)
        .arg("-x")
        .arg(region_arg.as_str())
        .arg(temp_file.path())
        .output()
        .map_err(|err| format!("screencapture unavailable at '{MACOS_SCREENCAPTURE_PATH}': {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(output.stderr.as_slice());
        let error_text = stderr.trim();
        return Err(format!(
            "screencapture failed for region {region_arg} (status {}): {}",
            output.status,
            if error_text.is_empty() {
                "unknown screencapture error"
            } else {
                error_text
            }
        ));
    }

    // Decode while the temp file still exists; the guard removes it on drop.
    load_macos_capture_image(temp_file.path(), &region_arg)
}

/// Fallback macOS main-display bounds, in *device pixels*, for the capture overlay.
///
/// screencapture has no geometry-print mode, so the main display is captured to
/// a temporary PNG and only its dimensions are read (via the PNG header, without
/// a full decode). The bounds use origin `(0, 0)` and the PNG's pixel size.
///
/// This is a FALLBACK only. The returned size is in device pixels, but the
/// overlay chain and `screencapture -R` work in logical points, so on a Retina
/// (2x) display these bounds are 2x too large. The launcher normally sources the
/// overlay bounds from egui's point-based monitor size and calls this helper only
/// when that size is unavailable (see `resolve_capture_desktop_bounds`). It also
/// covers only the main display; extra monitors are not resolved here.
///
/// # Errors
/// Returns a user-facing error if `screencapture` is missing, exits non-zero, or
/// its output dimensions cannot be read or are empty.
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn query_virtual_desktop_bounds_macos() -> Result<ScreenRect, String> {
    ensure_macos_screencapture_available()?;

    let temp_file = MacosCaptureTempFile::new(unique_macos_capture_path());

    // Full-screen (main display) capture: no `-R` means the whole main display.
    let output = Command::new(MACOS_SCREENCAPTURE_PATH)
        .arg("-x")
        .arg(temp_file.path())
        .output()
        .map_err(|err| format!("screencapture unavailable at '{MACOS_SCREENCAPTURE_PATH}': {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(output.stderr.as_slice());
        let error_text = stderr.trim();
        return Err(format!(
            "screencapture failed to capture the main display (status {}): {}",
            output.status,
            if error_text.is_empty() {
                "unknown screencapture error"
            } else {
                error_text
            }
        ));
    }

    // Read only the PNG header dimensions instead of decoding the whole image.
    let (width, height) = image::image_dimensions(temp_file.path()).map_err(|err| {
        format!(
            "failed to read screencapture main-display dimensions from '{}': {err}",
            temp_file.path().display()
        )
    })?;

    if width == 0 || height == 0 {
        return Err("screencapture returned an empty main display image".to_string());
    }

    Ok(ScreenRect {
        x: 0,
        y: 0,
        width,
        height,
    })
}

#[cfg(all(test, target_os = "macos", not(target_arch = "wasm32")))]
mod macos_tests {
    use super::{ScreenRect, build_screencapture_region_arg};

    #[test]
    fn region_arg_formats_points_as_x_y_w_h() {
        let rect = ScreenRect {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        assert_eq!(build_screencapture_region_arg(rect), "-R100,200,640,480");
    }

    #[test]
    fn region_arg_preserves_negative_origin() {
        // Displays left of / above the main display have negative point origins.
        let rect = ScreenRect {
            x: -1920,
            y: -50,
            width: 1920,
            height: 1080,
        };
        assert_eq!(build_screencapture_region_arg(rect), "-R-1920,-50,1920,1080");
    }
}
