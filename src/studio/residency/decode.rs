/*
File: src/studio/residency/decode.rs

Purpose:
Worker-side image decoding for the page residency pipeline: source pages are decoded and cut into
GPU-sized tiles, clean overlays are decoded and prepared for `CleanOverlaysModel`, and the page
cache loader decodes plain RGBA. Runs ONLY on loader worker threads, never on the GUI thread.

Key structures:
- `DecodedTile` / `DecodedPage`: a decoded source page cut into `DECODE_TILE_SIDE` tiles (plus the
  optional full RGBA that seeds the shared page cache), before GPU upload.
- `PreparedOverlay`: a decoded clean overlay in both forms the model keeps (straight `RgbaImage` +
  premultiplied `ColorImage`).

Key functions:
- `decode_page`, `decode_overlay`, `decode_page_rgba`: the three decode entry points.
- `decode_image_rgba`: the common path — optional experimental ffmpeg GPU decode, else the storage
  seam read + `image` decode.
- `copy_rgba_tile_rows`: copies one tile out of a decoded page.

Notes:
The experimental ffmpeg path is env opt-in (`MF_GPU_DECODE`, `MF_GPU_DECODE_MIN_PIXELS`) and has
no platform capability gate; it is skipped when ffmpeg is not runnable.
*/

use crate::runtime_log;
use eframe::egui::ColorImage;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

/// Maximum side of one GPU tile of a source page, in pixels.
const DECODE_TILE_SIDE: u32 = 2048;
const GPU_DECODE_DEFAULT_MIN_PIXELS: u64 = 6_000_000;
const ENV_GPU_DECODE: &str = "MF_GPU_DECODE";
const ENV_GPU_DECODE_MIN_PIXELS: &str = "MF_GPU_DECODE_MIN_PIXELS";

/// One tile of a decoded source page: straight RGBA rows of `size_px` at `origin_px` (page pixels).
pub(super) struct DecodedTile {
    pub(super) origin_px: [u32; 2],
    pub(super) size_px: [u32; 2],
    pub(super) rgba: Vec<u8>,
}

/// A decoded source page waiting for in-order promotion and GPU upload. `cache_rgba` is the full
/// image when the initial load seeds the shared page RGBA cache, else `None`.
pub(super) struct DecodedPage {
    pub(super) idx: usize,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) tiles: Vec<DecodedTile>,
    pub(super) cache_rgba: Option<Arc<image::RgbaImage>>,
}

/// A clean overlay prepared worker-side in both forms `CleanOverlaysModel` keeps, so the GUI
/// thread only moves it into the model.
pub(super) struct PreparedOverlay {
    pub(super) rgba: Arc<image::RgbaImage>,
    pub(super) color: ColorImage,
}

/// Decodes source page `idx` at `path` and cuts it into tiles of at most `DECODE_TILE_SIDE`.
/// `cache_page_rgba` keeps the full image for the shared page cache. Errors are display strings
/// naming the page index.
pub(super) fn decode_page(idx: usize, path: &Path, cache_page_rgba: bool) -> Result<DecodedPage, String> {
    let rgba = decode_image_rgba(idx, path)?;
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Ok(DecodedPage {
            idx,
            width: w,
            height: h,
            tiles: Vec::new(),
            cache_rgba: cache_page_rgba.then(|| Arc::new(rgba)),
        });
    }

    let mut tiles = Vec::new();
    let mut y = 0u32;
    while y < h {
        let mut x = 0u32;
        while x < w {
            let tw = (w - x).min(DECODE_TILE_SIDE);
            let th = (h - y).min(DECODE_TILE_SIDE);
            tiles.push(DecodedTile {
                origin_px: [x, y],
                size_px: [tw, th],
                rgba: copy_rgba_tile_rows(&rgba, x, y, tw, th),
            });
            x += DECODE_TILE_SIDE;
        }
        y += DECODE_TILE_SIDE;
    }

    Ok(DecodedPage {
        idx,
        width: w,
        height: h,
        tiles,
        cache_rgba: cache_page_rgba.then(|| Arc::new(rgba)),
    })
}

/// Decodes the clean overlay of page `idx` at `path` into a [`PreparedOverlay`].
pub(super) fn decode_overlay(idx: usize, path: &Path) -> Result<PreparedOverlay, String> {
    let image = decode_image_rgba(idx, path)?;
    let size = [image.width() as usize, image.height() as usize];
    Ok(PreparedOverlay {
        color: ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        rgba: Arc::new(image),
    })
}

/// Decodes source page `idx` at `path` into plain RGBA for the shared page cache.
pub(super) fn decode_page_rgba(idx: usize, path: &Path) -> Result<image::RgbaImage, String> {
    decode_image_rgba(idx, path)
}

fn decode_image_rgba(idx: usize, path: &Path) -> Result<image::RgbaImage, String> {
    match try_decode_image_rgba_with_ffmpeg(path) {
        Ok(Some(decoded)) => return Ok(decoded),
        Ok(None) => {}
        Err(err) => {
            runtime_log::log_warn(format!(
                "[residency] ffmpeg page decode failed; falling back to the CPU decoder; page_idx={idx}; path={}; error={err}",
                path.display()
            ));
        }
    }
    // Read bytes through the storage seam (native passthrough = same file on
    // disk; web = in-memory/IndexedDB store) then decode in memory. This is what
    // lets page images load in the browser build.
    let bytes = crate::storage::storage()
        .read(path.to_string_lossy().as_ref())
        .map_err(|e| format!("idx={idx} read failed: {e}"))?;
    image::load_from_memory(&bytes)
        .map_err(|e| format!("idx={idx} decode failed: {e}"))
        .map(|img| img.to_rgba8())
}

fn try_decode_image_rgba_with_ffmpeg(path: &Path) -> Result<Option<image::RgbaImage>, String> {
    // The header is parsed once here (via `should_try_gpu_decode`) and the resulting
    // dimensions are reused for the raw-output size check below, instead of re-reading the
    // image header a second time.
    let Some((width, height)) = should_try_gpu_decode(path) else {
        return Ok(None);
    };
    if width == 0 || height == 0 {
        return Ok(Some(image::RgbaImage::new(width, height)));
    }

    let output = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-hwaccel")
        .arg("auto")
        .arg("-i")
        .arg(path)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-")
        .output()
        .map_err(|e| format!("failed to launch ffmpeg: {e}"))?;

    if !output.status.success() {
        return Err(format!("ffmpeg exited with status {}", output.status));
    }

    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if output.stdout.len() != expected {
        return Err(format!(
            "ffmpeg raw output size mismatch: got {}, expected {}",
            output.stdout.len(),
            expected
        ));
    }

    image::RgbaImage::from_raw(width, height, output.stdout)
        .map(Some)
        .ok_or_else(|| "ffmpeg returned invalid RGBA buffer".to_string())
}

/// Decides whether to attempt experimental ffmpeg GPU decoding for `path`.
///
/// Returns `Some((width, height))` with the parsed header dimensions when GPU decode should
/// be tried, or `None` otherwise. The caller reuses the returned dimensions so the image
/// header is parsed only once across the decode-decision and decode-validation steps.
fn should_try_gpu_decode(path: &Path) -> Option<(u32, u32)> {
    if !gpu_decode_requested() || !ffmpeg_available() || !is_gpu_decode_extension_supported(path) {
        return None;
    }
    let (width, height) = image::image_dimensions(path).ok()?;
    let pixels = (width as u64).saturating_mul(height as u64);
    if pixels >= gpu_decode_min_pixels() {
        Some((width, height))
    } else {
        None
    }
}

fn is_gpu_decode_extension_supported(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "webp"
    )
}

fn gpu_decode_requested() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_var_bool(ENV_GPU_DECODE))
}

fn gpu_decode_min_pixels() -> u64 {
    static MIN_PIXELS: OnceLock<u64> = OnceLock::new();
    *MIN_PIXELS.get_or_init(|| {
        env::var(ENV_GPU_DECODE_MIN_PIXELS)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(GPU_DECODE_DEFAULT_MIN_PIXELS)
    })
}

fn ffmpeg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

fn env_var_bool(name: &str) -> bool {
    let Ok(raw) = env::var(name) else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn copy_rgba_tile_rows(
    rgba: &image::RgbaImage,
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let src = rgba.as_raw();
    let src_stride = rgba.width() as usize * 4;
    let dst_stride = width as usize * 4;
    let mut out = vec![0u8; dst_stride.saturating_mul(height as usize)];

    for row in 0..height as usize {
        let src_start = (origin_y as usize + row)
            .saturating_mul(src_stride)
            .saturating_add(origin_x as usize * 4);
        let src_end = src_start.saturating_add(dst_stride);
        let dst_start = row.saturating_mul(dst_stride);
        let dst_end = dst_start.saturating_add(dst_stride);
        out[dst_start..dst_end].copy_from_slice(&src[src_start..src_end]);
    }

    out
}
