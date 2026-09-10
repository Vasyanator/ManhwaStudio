/*
File: cleaning/tools/ai_editor/engines/lama/wire.rs

Purpose:
The engine's whole contract with the Python AI backend: the request header of each of the
two inpaint methods, the blob encoding they carry, the blocking calls themselves and the
`CallError` mapping. Everything here runs on a WORKER thread.

Main responsibilities:
- decide the effective `refine` flag (`effective_refine`) — the one place the rule lives;
- build the per-method request header (`lama_run_header`);
- run one inpaint pass (`run_lama`) and one unload (`unload_lama`);
- encode the region PNG and the L8 mask PNG into the two-image request blob.

Key functions:
- `effective_refine()`, `lama_run_header()`, `run_lama()`, `unload_lama()`

Notes:
The wire belongs to the BACKEND, not to this engine: `inpaint.lama_v2` / `inpaint.lama_mpe`,
the `image_len` / `mask_len` header ints, the `image_png ++ mask_png` request blob, the result
PNG in the RESPONSE BLOB, and the 300 s timeout. Changing any of it is a `PROTOCOL_VERSION`
change agreed with the Python side, never an engine-local edit.

`refine` may only ever travel as `true` for a catalog entry whose `supports_refine` is
`true`: on a TorchScript checkpoint the backend raises rather than refining, so sending it
would turn a user's leftover checkbox into a failed run. [`effective_refine`] is that gate
and is used by the header builder itself, so no call site can bypass it.
*/

use super::*;

/// The `refine` flag that actually goes on the wire.
///
/// `true` only when the user asked for it AND the selected checkpoint supports it. The
/// backend refuses its refine pass on a TorchScript `.pt` and LaMa-MPE has no refine pass at
/// all, so a stale checkbox must not reach either.
#[must_use]
pub(super) fn effective_refine(spec: &LamaModelSpec, settings: &LamaSettings) -> bool {
    spec.supports_refine && settings.refine
}

/// Builds the request header of one inpaint pass for `spec`'s method.
///
/// `image_len` / `mask_len` name the two segments of the request blob so the backend can
/// split it. The `params` object differs per method and carries only the fields that method
/// reads: sending an MPE parameter to `inpaint.lama_v2` (or the reverse) would be a field
/// the handler does not know.
#[must_use]
pub(super) fn lama_run_header(
    spec: &LamaModelSpec,
    settings: &LamaSettings,
    image_len: usize,
    mask_len: usize,
) -> Value {
    let params = match spec.method {
        LamaMethod::V2 => json!({
            "refine": effective_refine(spec, settings),
            "n_iters": settings.n_iters,
            "max_scales": settings.max_scales,
            "px_budget": settings.px_budget,
            "model_name": spec.file_name,
        }),
        // No `model_name`: the MPE method names its own checkpoint, and the handler
        // accepts `inpaint_size` alone.
        LamaMethod::Mpe => json!({ "inpaint_size": settings.inpaint_size }),
    };
    json!({
        "image_len": image_len,
        "mask_len": mask_len,
        "params": params,
    })
}

/// Runs one inpaint pass and returns the result image, exactly the size of `region`.
///
/// Blocking, and therefore worker-thread only. It also performs the ensure-before-run step,
/// which may download the checkpoint.
///
/// `mask` is the host's painted layer as L8 bytes, exactly `region.size[0] * region.size[1]`
/// of them. It is put on the wire verbatim: the meaning is "remove what is under it".
///
/// # Errors
/// Returns a localized message when the mask does not match the region, when the model
/// cannot be fetched, when the backend refuses the call, when the response carries no PNG,
/// or when the returned image is not the size that was asked for.
pub(super) fn run_lama(
    region: &egui::ColorImage,
    mask: &[u8],
    spec: &LamaModelSpec,
    settings: &LamaSettings,
) -> Result<egui::ColorImage, String> {
    let (width, height) = (region.size[0], region.size[1]);
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    ensure_lama_model_ready(spec)?;

    let image_png = encode_color_image_png_rgba(region)?;
    let mask_png = encode_mask_png_l8(mask, width, height)?;
    let header = lama_run_header(spec, settings, image_png.len(), mask_png.len());
    let blob = concat_image_mask(&image_png, &mask_png);

    let (_response_header, out_bytes) = inpaint_call(spec.method.call_method(), header, &blob)?;
    if out_bytes.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    let out_rgba = image::load_from_memory(&out_bytes)
        .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
        .to_rgba8();
    let out_w = usize::try_from(out_rgba.width()).unwrap_or(usize::MAX);
    let out_h = usize::try_from(out_rgba.height()).unwrap_or(usize::MAX);
    // Strict equality, never a rescale: the host writes the result back into exactly the
    // frame rectangle it handed over, so an image of another size is a failure, not a hint.
    if out_w != width || out_h != height {
        return Err(tf!("cleaning.inpaint.unexpected_size_error", out_w = out_w, out_h = out_h, width = width, height = height));
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [out_w, out_h],
        out_rgba.as_raw(),
    ))
}

/// Asks the backend to drop the model of `method`. Blocking, worker-thread only.
///
/// # Errors
/// Returns a localized message when the call fails.
pub(super) fn unload_lama(method: LamaMethod) -> Result<(), String> {
    let (header, _blob) = inpaint_call(method.unload_method(), json!({}), &[])?;
    // The backend answers `{ "unloaded": bool }`. `false` means "nothing was resident",
    // which is not a failure and has nothing to report beyond the confirmation the caller
    // already prints.
    let _unloaded = header.get("unloaded").and_then(Value::as_bool);
    Ok(())
}

/// Issues one blocking v2 framed call. The result PNG comes from the RESPONSE BLOB (raw
/// bytes) and the metadata from the response header.
fn inpaint_call(method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let client =
        backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(method, header, blob, LAMA_BACKEND_CALL_TIMEOUT)
        .map_err(map_inpaint_call_error)
}

/// `CallError` -> user-facing string.
///
/// - `Error`       -> the backend's own message, verbatim.
/// - `Interrupted` -> the transient-abort wording.
/// - `Transport`   -> the unified "backend offline" message, because a framing or connect
///   failure means the process is not there, not that the model failed.
fn map_inpaint_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

/// Concatenates the region PNG and the mask PNG into the two-image request blob. The
/// receiver splits it by the `image_len` / `mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

/// Encodes the region as an RGBA8 PNG.
///
/// # Errors
/// Returns a localized message when a side does not fit `u32` or the encoder fails.
fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let (width, height) = (image.size[0], image.size[1]);
    let width_u32 = u32::try_from(width)
        .map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;
    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::Rgba8.into())
        .map_err(|err| tf!("cleaning.png.encode_image_error", err = err))?;
    Ok(out)
}

/// Encodes the removal mask as an L8 PNG. `mask` must be exactly `width * height` bytes.
///
/// # Errors
/// Returns a localized message when a side does not fit `u32`, when `mask` is not the
/// expected length, or when the encoder fails.
fn encode_mask_png_l8(mask: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let width_u32 = u32::try_from(width)
        .map_err(|_| t!("cleaning.png.mask_width_too_large_error").to_string())?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| t!("cleaning.png.mask_height_too_large_error").to_string())?;
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(mask, width_u32, height_u32, ColorType::L8.into())
        .map_err(|err| tf!("cleaning.png.encode_mask_error", err = err))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static LamaModelSpec {
        lama_model_spec_by_name(name).expect("catalog entry")
    }

    /// The request blob is `image_png ++ mask_png` in that order, and the two header ints
    /// name the segment lengths the backend splits it by.
    #[test]
    fn blob_concat_orders_image_then_mask_with_lengths() {
        let image_png = b"IMAGE_PNG_BYTES".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);
        let header = lama_run_header(
            spec("best.ckpt"),
            &LamaSettings::default(),
            image_png.len(),
            mask_png.len(),
        );
        assert_eq!(header["image_len"].as_u64(), Some(15));
        assert_eq!(header["mask_len"].as_u64(), Some(4));
        assert_eq!(blob.len(), image_png.len() + mask_png.len());
        assert_eq!(&blob[..image_png.len()], image_png.as_slice());
        assert_eq!(&blob[image_png.len()..], mask_png.as_slice());
    }

    /// A LaMa-v2 entry carries the four refine fields plus the checkpoint name; the MPE
    /// entry carries `inpaint_size` alone and NO `model_name`, because its handler names
    /// its own file.
    #[test]
    fn each_method_carries_only_its_own_parameters() {
        let settings = LamaSettings::default();
        let v2 = lama_run_header(spec("best.ckpt"), &settings, 1, 1);
        assert_eq!(v2["params"]["model_name"].as_str(), Some("best.ckpt"));
        assert_eq!(v2["params"]["n_iters"].as_u64(), Some(15));
        assert_eq!(v2["params"]["max_scales"].as_u64(), Some(3));
        assert_eq!(v2["params"]["px_budget"].as_u64(), Some(1_000_000));
        assert!(v2["params"].get("inpaint_size").is_none());

        let mpe = lama_run_header(spec("inpainting_lama_mpe.ckpt"), &settings, 1, 1);
        assert_eq!(mpe["params"]["inpaint_size"].as_u64(), Some(2048));
        assert!(
            mpe["params"].get("model_name").is_none(),
            "`inpaint.lama_mpe` takes no model name"
        );
        assert!(mpe["params"].get("refine").is_none());
    }

    /// A model that cannot refine never sends `refine: true`, however the checkbox was
    /// left: the backend raises on a refine request against a TorchScript checkpoint.
    #[test]
    fn refine_never_reaches_the_wire_for_a_model_that_cannot_refine() {
        let asked = LamaSettings { refine: true, ..LamaSettings::default() };
        assert!(effective_refine(spec("best.ckpt"), &asked));
        assert!(!effective_refine(spec("anime-manga-big-lama.pt"), &asked));
        assert!(!effective_refine(spec("inpainting_lama_mpe.ckpt"), &asked));

        let torchscript = lama_run_header(spec("anime-manga-big-lama.pt"), &asked, 1, 1);
        assert_eq!(torchscript["params"]["refine"].as_bool(), Some(false));
        let ckpt = lama_run_header(spec("best.ckpt"), &asked, 1, 1);
        assert_eq!(ckpt["params"]["refine"].as_bool(), Some(true));

        // And the other direction: an unchecked box never becomes `true` on a model that
        // could have refined.
        let unchecked = LamaSettings::default();
        assert!(!effective_refine(spec("best.ckpt"), &unchecked));
    }

    /// The mask must be exactly one byte per region pixel; anything else is refused before
    /// it can be encoded into a PNG the backend would misread.
    #[test]
    fn a_mask_of_the_wrong_length_is_refused() {
        assert!(encode_mask_png_l8(&[0u8; 5], 2, 2).is_err());
        assert!(encode_mask_png_l8(&[0u8; 4], 2, 2).is_ok());
    }

    /// `CallError` mapping keeps the backend message verbatim, pins the interrupt wording
    /// and turns any transport failure into the unified offline message.
    #[test]
    fn call_error_mapping_preserves_messages() {
        assert_eq!(map_inpaint_call_error(CallError::Error("boom".to_string())), "boom");
        assert_eq!(
            map_inpaint_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_inpaint_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }
}
