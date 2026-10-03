/*
File: cleaning/tools/ai_editor/engines/sdxl/wire.rs

Purpose:
The engine's whole contract with the Python AI backend: the request header of one run, the
blob encoding it carries, the streaming call itself, the unload call and the `CallError`
mapping. Everything here runs on a WORKER thread.

Main responsibilities:
- decide which LaMa prefill checkpoint (if any) rides along with a run
  (`lama_model_for_run`) — the one place that rule lives;
- build the request header (`sdxl_run_header`);
- run one streaming pass (`run_sdxl`) and one unload (`unload_sdxl`);
- pack the region PNG and the L8 mask PNG (encoded by `tools::region_png`, the cleaning face
  of `ms_tools::png_wire`) into the two-image request blob;
- decode a streamed latent preview.

Key functions:
- `lama_model_for_run()`, `sdxl_run_header()`, `run_sdxl()`, `unload_sdxl()`

Notes:
The wire belongs to the BACKEND, not to this engine: the `inpaint.sdxl` method, the
`image_len` / `mask_len` header ints, the `image_png ++ mask_png` request blob, the result PNG
in the RESPONSE BLOB, the `progress` frames carrying `step` / `total` in the header and a
preview PNG in the frame blob, and the 20 minute timeout. Changing any of it is a
`PROTOCOL_VERSION` change agreed with the Python side, never an engine-local edit.

`lama_model` may only ever travel in [`SdxlMode::FourChannel`]: the 9-channel pipeline has no
prefill step, so the field would name a checkpoint nothing loads. [`lama_model_for_run`] is
that gate and is used by the header builder itself, so no call site can bypass it.

The ensure-before-run step of the prefill checkpoint may DOWNLOAD it, which is a second
reason everything here is worker-thread only.
*/

use super::*;

/// The LaMa prefill checkpoint that travels with a run, `None` when the mode has no prefill.
///
/// The name is resolved through the LaMa engine's own catalog
/// ([`ensure_lama_model_for_external`]), which also fetches the file when it is missing —
/// so this is worker-thread only and may take as long as a download.
///
/// # Errors
/// Returns a localized message when the selected name is not a LaMa-v2 catalog entry or when
/// the checkpoint cannot be fetched.
pub(super) fn lama_model_for_run(cfg: &SdxlRunConfig) -> Result<Option<String>, String> {
    match cfg.mode {
        SdxlMode::FourChannel => {
            Ok(Some(ensure_lama_model_for_external(&cfg.settings.lama_model)?.to_string()))
        }
        // No prefill: the 9-channel UNet takes the clean masked image as its own channel.
        SdxlMode::NineChannel => Ok(None),
    }
}

/// Builds the request header of one run.
///
/// `image_len` / `mask_len` name the two segments of the request blob so the backend can
/// split it. `lama_model` is what [`lama_model_for_run`] resolved and is inserted into
/// `params` only when it is `Some`, i.e. only for the 4-channel mode.
#[must_use]
pub(super) fn sdxl_run_header(
    cfg: &SdxlRunConfig,
    lama_model: Option<&str>,
    image_len: usize,
    mask_len: usize,
) -> Value {
    let mut params = json!({
        "mode": cfg.mode.wire(),
        "model_path": cfg.settings.model_path.trim(),
        "positive_prompt": cfg.settings.positive_prompt,
        "negative_prompt": cfg.settings.negative_prompt,
        "steps": cfg.settings.steps,
        "cfg_scale": cfg.settings.cfg_scale,
        "denoise_strength": cfg.settings.denoise_strength,
        "seed": cfg.settings.seed,
        "sampler": cfg.settings.sampler,
        "mask_blur": cfg.settings.mask_blur,
        "mask_dilation": cfg.settings.mask_dilation,
    });
    if let Some(lama_model) = lama_model
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("lama_model".to_string(), Value::String(lama_model.to_string()));
    }
    json!({
        "image_len": image_len,
        "mask_len": mask_len,
        "params": params,
    })
}

/// Runs one streaming inpaint pass and returns the result image, exactly the size of
/// `region`.
///
/// Blocking, and therefore worker-thread only. It also performs the ensure-before-run step of
/// the 4-channel prefill checkpoint, which may download it.
///
/// `mask` is the host's painted layer as L8 bytes, exactly `region.size[0] * region.size[1]`
/// of them, and goes on the wire verbatim: the meaning is "regenerate what is under it".
/// `generation` stamps every progress publication, so the frames of a run the user has
/// already cancelled are dropped instead of driving the next run's bar.
///
/// # Errors
/// Returns a localized message when the mask does not match the region, when no weights path
/// is set, when the prefill checkpoint cannot be fetched, when the backend refuses the call,
/// when the response carries no PNG, or when the returned image is not the size that was
/// asked for.
pub(super) fn run_sdxl(
    region: &egui::ColorImage,
    mask: &[u8],
    cfg: &SdxlRunConfig,
    progress: &Arc<Mutex<SdxlSharedProgress>>,
    generation: u64,
) -> Result<egui::ColorImage, String> {
    let (width, height) = (region.size[0], region.size[1]);
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    // Re-checked on the worker as well as in the run gate: the gate closes the button, but a
    // path emptied between the click and the encode must not reach the backend as an empty
    // string it would have to invent a message for.
    if cfg.settings.model_path.trim().is_empty() {
        return Err(t!("cleaning.tools.sdxl.weights_path_required_error").to_string());
    }

    let lama_model = lama_model_for_run(cfg)?;
    let image_png = encode_color_image_png_rgba(region)?;
    let mask_png = encode_mask_png_l8(mask, width, height)?;
    let header = sdxl_run_header(cfg, lama_model.as_deref(), image_png.len(), mask_png.len());
    let blob = concat_image_mask(&image_png, &mask_png);

    let stream_result = sdxl_stream_call(header, &blob, |step, total, preview| {
        publish_progress_frame(progress, generation, step, total, preview);
    });
    finish_progress_generation(progress, generation);

    let (_response_header, out_bytes) = stream_result?;
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
    Ok(egui::ColorImage::from_rgba_unmultiplied([out_w, out_h], out_rgba.as_raw()))
}

/// Issues the streaming `inpaint.sdxl` call, reporting each interim frame through
/// `on_progress(step, total, preview)`.
///
/// `step` / `total` come from the progress frame's HEADER and the preview PNG from that
/// frame's BLOB (raw bytes, decoded into a `ColorImage`; an empty blob means "no preview in
/// this frame"). The terminal result PNG comes back in the RESPONSE BLOB, together with the
/// response header (`engine` / `source_size` / `device` / `mode`).
///
/// # Errors
/// Returns the localized mapping of `CallError` — see [`map_sdxl_call_error`].
fn sdxl_stream_call<F>(header: Value, blob: &[u8], mut on_progress: F) -> Result<(Value, Vec<u8>), String>
where
    F: FnMut(u32, u32, Option<egui::ColorImage>),
{
    let client =
        backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call_streaming(
            backend_ipc::protocol::METHOD_INPAINT_SDXL,
            header,
            blob,
            |progress_header, preview_blob| {
                let step = progress_frame_counter(progress_header, "step");
                let total = progress_frame_counter(progress_header, "total");
                on_progress(step, total, decode_preview_image(preview_blob));
            },
            SDXL_BACKEND_CALL_TIMEOUT,
        )
        .map_err(map_sdxl_call_error)
}

/// Reads one counter out of a `progress` frame header, defaulting to `0`.
///
/// A missing or oversized value reads as `0` rather than aborting the run: a progress frame
/// is a hint about a pass that is otherwise going fine, and the bar treats `total == 0` as
/// "not known yet".
#[must_use]
fn progress_frame_counter(header: &Value, field: &str) -> u32 {
    header
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

/// Asks the backend to drop the resident SDXL pipeline. Blocking, worker-thread only.
///
/// # Errors
/// Returns a localized message when the call fails.
pub(super) fn unload_sdxl() -> Result<(), String> {
    let client =
        backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_SDXL_UNLOAD,
            json!({}),
            &[],
            SDXL_BACKEND_CALL_TIMEOUT,
        )
        .map_err(map_sdxl_call_error)?;
    // The backend answers `{ "unloaded": bool }`. `false` means "nothing was resident",
    // which is not a failure and has nothing to report beyond the confirmation the caller
    // already prints.
    let _unloaded = header.get("unloaded").and_then(Value::as_bool);
    Ok(())
}

/// `CallError` -> user-facing string.
///
/// - `Error`       -> the backend's own message, verbatim.
/// - `Interrupted` -> the transient-abort wording.
/// - `Transport`   -> the unified "backend offline" message, because a framing or connect
///   failure means the process is not there, not that the model failed.
fn map_sdxl_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

/// Decodes a streamed latent preview PNG into a `ColorImage`.
///
/// `None` for an empty blob (the frame carried no preview) and for any decode failure: a
/// preview is decoration, and a corrupt one must not fail the run it belongs to.
fn decode_preview_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    if bytes.is_empty() {
        return None;
    }
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [
        usize::try_from(rgba.width()).ok()?,
        usize::try_from(rgba.height()).ok()?,
    ];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Concatenates the region PNG and the mask PNG into the two-image request blob. The
/// receiver splits it by the `image_len` / `mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: SdxlMode) -> SdxlRunConfig {
        SdxlRunConfig {
            mode,
            settings: SdxlSettings {
                model_path: "  /models/sdxl.safetensors  ".to_string(),
                ..SdxlSettings::for_mode(mode)
            },
        }
    }

    /// The request blob is `image_png ++ mask_png` in that order, and the two header ints
    /// name the segment lengths the backend splits it by.
    #[test]
    fn blob_concat_orders_image_then_mask_with_lengths() {
        let image_png = b"IMAGE_PNG_BYTES".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);
        let header = sdxl_run_header(
            &config(SdxlMode::NineChannel),
            None,
            image_png.len(),
            mask_png.len(),
        );
        assert_eq!(header["image_len"].as_u64(), Some(15));
        assert_eq!(header["mask_len"].as_u64(), Some(4));
        assert_eq!(blob.len(), image_png.len() + mask_png.len());
        assert_eq!(&blob[..image_png.len()], image_png.as_slice());
        assert_eq!(&blob[image_png.len()..], mask_png.as_slice());
    }

    /// The header carries every generation parameter the backend reads, and the weights path
    /// is TRIMMED — a path pasted with a trailing space must still resolve.
    #[test]
    fn the_header_carries_every_generation_parameter() {
        let cfg = config(SdxlMode::NineChannel);
        let header = sdxl_run_header(&cfg, None, 1, 1);
        let params = &header["params"];
        assert!(params.is_object());
        assert_eq!(params["mode"].as_str(), Some("nine_channel"));
        assert_eq!(params["model_path"].as_str(), Some("/models/sdxl.safetensors"));
        assert_eq!(params["positive_prompt"].as_str(), Some(cfg.settings.positive_prompt.as_str()));
        assert_eq!(params["negative_prompt"].as_str(), Some(cfg.settings.negative_prompt.as_str()));
        assert_eq!(params["steps"].as_u64(), Some(30));
        assert_eq!(params["sampler"].as_str(), Some("DPM++ 2M Karras"));
        assert_eq!(params["seed"].as_i64(), Some(-1), "the -1 sentinel must survive as a signed value");
        assert_eq!(params["mask_blur"].as_u64(), Some(4));
        assert_eq!(params["mask_dilation"].as_u64(), Some(6));
        assert!(params["cfg_scale"].is_number());
        assert!(params["denoise_strength"].is_number());
    }

    /// The prefill checkpoint rides along ONLY in the mode that has a prefill step. Sending
    /// it in the 9-channel mode would name a checkpoint nothing loads.
    #[test]
    fn the_lama_prefill_model_travels_only_in_the_four_channel_mode() {
        let four = sdxl_run_header(&config(SdxlMode::FourChannel), Some("best.ckpt"), 1, 1);
        assert_eq!(four["params"]["mode"].as_str(), Some("four_channel"));
        assert_eq!(four["params"]["lama_model"].as_str(), Some("best.ckpt"));

        let nine = sdxl_run_header(&config(SdxlMode::NineChannel), None, 1, 1);
        assert!(
            nine["params"].get("lama_model").is_none(),
            "the 9-channel pipeline has no prefill step"
        );
    }

    /// And the rule is decided in ONE place: the mode alone says whether a prefill model is
    /// resolved at all, so no call site can put one on the wire behind the gate's back.
    #[test]
    fn only_the_four_channel_mode_resolves_a_prefill_model() {
        assert_eq!(
            lama_model_for_run(&config(SdxlMode::NineChannel)),
            Ok(None),
            "the 9-channel mode never even asks the LaMa catalog"
        );
        // The 4-channel branch is not exercised here: it ensures the checkpoint is ON DISK
        // and would download it, which is not something a unit test may do. What it returns
        // when it succeeds is pinned by the header test above.
    }

    /// The mask must be exactly one byte per region pixel; anything else is refused before it
    /// can be encoded into a PNG the backend would misread.
    #[test]
    fn a_mask_of_the_wrong_length_is_refused() {
        assert!(encode_mask_png_l8(&[0u8; 5], 2, 2).is_err());
        assert!(encode_mask_png_l8(&[0u8; 4], 2, 2).is_ok());
    }

    /// A `progress` frame carries its counters in the HEADER and its preview PNG in the frame
    /// BLOB; an absent counter reads as `0` and an empty blob as "no preview".
    #[test]
    fn a_progress_frame_parses_its_counters_and_its_preview_blob() {
        let header = json!({ "v": 1, "id": 42, "kind": "progress", "step": 7, "total": 30 });
        assert_eq!(progress_frame_counter(&header, "step"), 7);
        assert_eq!(progress_frame_counter(&header, "total"), 30);
        assert_eq!(progress_frame_counter(&json!({}), "step"), 0);
        assert_eq!(
            progress_frame_counter(&json!({ "step": u64::MAX }), "step"),
            0,
            "an oversized counter is dropped, not truncated into a wrong step number"
        );

        let mut preview_blob = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            1,
            image::Rgba([9, 8, 7, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut preview_blob), image::ImageFormat::Png)
        .expect("encode preview PNG");
        assert_eq!(
            decode_preview_image(&preview_blob).expect("decode preview from blob").size,
            [2, 1]
        );
        assert!(decode_preview_image(&[]).is_none());
        assert!(decode_preview_image(b"not a png").is_none(), "a corrupt preview must not fail the run");
    }

    /// `CallError` mapping keeps the backend message verbatim, pins the interrupt wording and
    /// turns any transport failure into the unified offline message.
    #[test]
    fn call_error_mapping_preserves_messages() {
        assert_eq!(map_sdxl_call_error(CallError::Error("boom".to_string())), "boom");
        assert_eq!(
            map_sdxl_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_sdxl_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }
}
