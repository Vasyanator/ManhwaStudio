/*
File: tools/watermark_removal/backend.rs

Purpose:
Python backend transport for the AI watermark modes: the run request and its header, the
streamed call, the result blob split / decode, model unload, and the tool's settings file.

Key items:
- `run_watermark()`, `watermark_request_header()`, `watermark_stream_call()`
- `split_watermark_remove_blob()`, `decode_region_rgba()`, `decode_region_mask()`
- `unload_watermark_model()`, `load_watermark_settings()`, `save_watermark_settings()`

Notes:
Called from worker threads only.
*/
use crate::tools::mask_generation::{
    WATERMARK_DETECT_DOWNSCALE_TO, WatermarkProgress, lock_watermark_progress,
    map_watermark_call_error,
};
use ms_backend_ipc as backend_ipc;
use ms_config as config;
use ms_local_ai::backend_health::ai_backend_offline_error;
use crate::tools::region_png::{decode_mask_png, encode_color_image_png_rgba};
use eframe::egui;
use egui::Color32;
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Mutex};
use super::{
    WATERMARK_RUN_CALL_TIMEOUT, WATERMARK_UNLOAD_CALL_TIMEOUT, WatermarkNetworkMode,
    WatermarkRemovalSettings, WatermarkRunOutcome,
};


/// Runs one watermark pass on `image` and returns the outcome.
///
/// `settings` must already be `normalized()`. Mask-only mode streams
/// `watermark.detect` and returns the mask alone; clean mode streams
/// `watermark.remove` and returns the reconstruction plus its mask. `progress` is
/// updated from the `progress` frames and is always cleared before returning,
/// including on failure, so the editor never keeps a stuck progress bar.
///
/// # Errors
/// Returns a user-facing message when the region is empty, when the backend
/// fails or is unreachable, when the response blob does not match its declared
/// lengths, or when a returned PNG has the wrong size.
pub(super) fn run_watermark(
    image: &egui::ColorImage,
    settings: &WatermarkRemovalSettings,
    progress: &Arc<Mutex<WatermarkProgress>>,
    mode: WatermarkNetworkMode,
) -> Result<WatermarkRunOutcome, String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Err(t!("cleaning.region.invalid_selection_size_error").to_string());
    }
    let image_png = encode_color_image_png_rgba(image)?;
    let header = watermark_request_header(mode, settings);

    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = true;
        guard.phase = "generate".to_string();
        guard.step = 0;
        guard.total = 0;
        guard.label = t!("cleaning.tools.watermark.preparing_status").to_string();
    }
    let stream_result = watermark_stream_call(
        mode.ipc_method(),
        header,
        &image_png,
        |phase, step, total, label| {
            let mut guard = lock_watermark_progress(progress);
            guard.phase = phase;
            guard.step = step;
            guard.total = total;
            guard.label = label;
        },
    );
    {
        let mut guard = lock_watermark_progress(progress);
        guard.active = false;
    }

    let (response_header, blob) = stream_result?;
    match mode {
        WatermarkNetworkMode::MaskOnly => Ok(WatermarkRunOutcome {
            image: None,
            mask: decode_region_mask(&blob, image.size)?,
        }),
        WatermarkNetworkMode::Clean => {
            let (clean_png, mask_png) = split_watermark_remove_blob(&response_header, &blob)?;
            Ok(WatermarkRunOutcome {
                image: Some(decode_region_rgba(clean_png, image.size)?),
                mask: decode_region_mask(mask_png, image.size)?,
            })
        }
    }
}

/// Builds the request header of a run. `model` is the wire id of the catalog
/// entry; the detect pass is not tiled (plan §3.3) and takes the shared downscale
/// target instead of the tile geometry.
pub(super) fn watermark_request_header(
    mode: WatermarkNetworkMode,
    settings: &WatermarkRemovalSettings,
) -> Value {
    match mode {
        WatermarkNetworkMode::MaskOnly => json!({
            "params": {
                "model": settings.model,
                "downscale_to": WATERMARK_DETECT_DOWNSCALE_TO,
                "threshold": settings.threshold,
                "dilate_px": settings.dilate_px,
            }
        }),
        WatermarkNetworkMode::Clean => json!({
            "params": {
                "model": settings.model,
                "tile": settings.tile,
                "overlap": settings.overlap,
                "threshold": settings.threshold,
                "dilate_px": settings.dilate_px,
            }
        }),
    }
}

/// Issues a streaming `watermark.*` request. Each `progress` frame carries
/// `phase`/`step`/`total`/`label` in its header and no blob.
///
/// # Errors
/// Returns the backend error message, the abort notice for an interrupted call,
/// or the unified offline message when the transport fails.
fn watermark_stream_call<F>(
    method: &str,
    header: Value,
    blob: &[u8],
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    F: FnMut(String, u64, u64, String),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call_streaming(
            method,
            header,
            blob,
            |progress_header, _preview_blob| {
                let phase = progress_header
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("generate")
                    .to_string();
                let step = progress_header
                    .get("step")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total = progress_header
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let label = progress_header
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                on_progress(phase, step, total, label);
            },
            WATERMARK_RUN_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)
}

/// Splits the `watermark.remove` response blob into `(clean_png, mask_png)`.
///
/// The blob is `clean_png ++ mask_png` and the header declares both lengths. They
/// are validated with STRICT equality against the blob length, so a truncated or
/// padded frame is rejected instead of being sliced into garbage.
///
/// # Errors
/// Returns a user-facing message when `image_len`/`mask_len` are missing, are not
/// non-negative integers, are zero, or do not add up to exactly `blob.len()`.
pub(super) fn split_watermark_remove_blob<'a>(
    header: &Value,
    blob: &'a [u8],
) -> Result<(&'a [u8], &'a [u8]), String> {
    let image_len = header_len_field(header, "image_len")?;
    let mask_len = header_len_field(header, "mask_len")?;
    if image_len == 0 || mask_len == 0 {
        return Err(t!("cleaning.tools.watermark.empty_result_error").to_string());
    }
    let expected = image_len
        .checked_add(mask_len)
        .ok_or_else(|| t!("cleaning.tools.watermark.blob_header_error").to_string())?;
    if expected != blob.len() {
        return Err(tf!(
            "cleaning.tools.watermark.blob_length_error",
            actual = blob.len(),
            expected = expected
        ));
    }
    Ok((&blob[..image_len], &blob[image_len..]))
}

/// Reads a `*_len` response-header field as a `usize`.
///
/// # Errors
/// Returns the malformed-header message when the field is missing, is not an
/// unsigned integer, or does not fit into `usize`.
fn header_len_field(header: &Value, field: &str) -> Result<usize, String> {
    header
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| t!("cleaning.tools.watermark.blob_header_error").to_string())
}

/// Decodes the reconstructed region PNG and checks it against the region size.
///
/// # Errors
/// Returns a user-facing message for an empty payload, a corrupt PNG, or a size
/// that does not match `expected`.
fn decode_region_rgba(bytes: &[u8], expected: [usize; 2]) -> Result<egui::ColorImage, String> {
    if bytes.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    let rgba = image::load_from_memory(bytes)
        .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
        .to_rgba8();
    let width = usize::try_from(rgba.width())
        .map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height = usize::try_from(rgba.height())
        .map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;
    if [width, height] != expected {
        return Err(tf!(
            "cleaning.inpaint.unexpected_size_error",
            out_w = width,
            out_h = height,
            width = expected[0],
            height = expected[1]
        ));
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        rgba.as_raw(),
    ))
}

/// Decodes the predicted L8 mask PNG into a binary mask in region coordinates
/// (opaque white = watermark, transparent = untouched).
///
/// # Errors
/// Returns a user-facing message for an empty payload, a mask whose size does not
/// match `expected`, or a buffer whose length does not match its own size.
fn decode_region_mask(bytes: &[u8], expected: [usize; 2]) -> Result<egui::ColorImage, String> {
    if bytes.is_empty() {
        return Err(t!("cleaning.tools.watermark.no_mask_result_error").to_string());
    }
    let method_label = t!("cleaning.mask_editor.source.watermark");
    let (mask_size, mask_alpha) = decode_mask_png(bytes)?;
    let width = usize::try_from(mask_size[0]).map_err(|_| {
        tf!(
            "cleaning.mask_editor.mask_width_too_large_error",
            method_label = method_label
        )
    })?;
    let height = usize::try_from(mask_size[1]).map_err(|_| {
        tf!(
            "cleaning.mask_editor.mask_height_too_large_error",
            method_label = method_label
        )
    })?;
    if [width, height] != expected {
        return Err(tf!(
            "cleaning.mask_editor.mask_size_error",
            method_label = method_label,
            mask_w = width,
            mask_h = height,
            image = expected[0],
            image_2 = expected[1]
        ));
    }
    let expected_len = width.saturating_mul(height);
    if expected_len != mask_alpha.len() {
        return Err(tf!(
            "cleaning.mask_editor.mask_length_error",
            method_label = method_label,
            actual_len = mask_alpha.len(),
            expected_len = expected_len
        ));
    }
    let mut mask = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    for (dst, alpha) in mask.pixels.iter_mut().zip(mask_alpha) {
        if alpha != 0 {
            *dst = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
        }
    }
    Ok(mask)
}

/// Asks the backend to drop the resident watermark model.
///
/// # Errors
/// Returns the backend error message, or the unified offline message when the
/// backend cannot be reached.
pub(super) fn unload_watermark_model() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            backend_ipc::protocol::METHOD_WATERMARK_UNLOAD,
            json!({}),
            &[],
            WATERMARK_UNLOAD_CALL_TIMEOUT,
        )
        .map_err(map_watermark_call_error)?;
    Ok(())
}

/// Loads the settings file, falling back to the defaults for a missing or
/// unreadable file and for JSON that does not parse.
pub(super) fn load_watermark_settings() -> WatermarkRemovalSettings {
    let path = config::watermark_removal_settings_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return WatermarkRemovalSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Writes the settings file, creating its parent directory if needed.
///
/// # Errors
/// Returns a user-facing message when the directory cannot be created, the value
/// cannot be serialized, or the file cannot be written.
pub(super) fn save_watermark_settings(settings: &WatermarkRemovalSettings) -> Result<(), String> {
    let path = config::watermark_removal_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|err| tf!("cleaning.tools.watermark.serialize_settings_error", err = err))?;
    fs::write(&path, raw)
        .map_err(|err| tf!("cleaning.tools.watermark.write_settings_error", err = err))
}
