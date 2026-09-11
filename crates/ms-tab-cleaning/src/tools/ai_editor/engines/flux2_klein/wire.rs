/*
File: cleaning/tools/ai_editor/engines/flux2_klein/wire.rs

Purpose:
Every backend call the FLUX.2 klein engine makes that is not a download or a prompt-cache
operation: the generation run itself with its OOM retry pass, the streaming call helper
the long operations share, the `.status` query and its parsers, the component
load/unload actions, and the encoding of the image/mask blob that travels with a run.

Main responsibilities:
- run one generation pass and, on an OOM, the recovery pass (`run_flux2_klein`,
  `run_flux2_klein_pass`);
- drive a streamed IPC call and hand every frame to the caller (`flux2_stream_call`);
- query the component catalog and parse the answer (`fetch_flux2_status`,
  `parse_flux2_status`, `parse_flux2_component_snapshot`);
- request a component load/unload and read the catalog it answers with;
- translate a prompt to English through the shared MT service;
- encode the colour image and the mask into the single PNG blob the request carries.

Key functions:
- `run_flux2_klein()`, `run_flux2_klein_pass()`, `flux2_stream_call()`
- `flux2_status_header()`, `fetch_flux2_status()`, `parse_flux2_status()`,
  `parse_flux2_component_snapshot()`
- `unload_flux2_klein()`, `flux2_component_action_header()`,
  `run_flux2_component_action()`
- `translate_prompt_to_english()`, `map_flux2_call_error()`
- `concat_image_mask()`, `encode_color_image_png_rgba()`, `encode_mask_png_l8()`

Notes:
Every function here runs on a WORKER thread — none of it may be called from the GUI
thread. `FLUX2_MAX_SEQ` is pinned at 512 and is part of the prompt-cache key: lowering
it invalidates the whole saved `.msprompt` library at once.
*/

use super::*;

/// Prompt token budget put on the wire, PINNED and no longer a setting.
///
/// It is also the maximum the backend accepts, so every value a user could have chosen
/// was a reduction; worse, the length is part of the prompt-cache key, so lowering it
/// invalidated every `.msprompt` in the saved library at once (the backend refuses a cache
/// built at another length). The constant stays because the wire field stays.
pub(super) const FLUX2_MAX_SEQ: u32 = 512;

/// Generation may load ~20 GB of weights before the first step; allow a wide window.
pub(super) const FLUX2_RUN_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// `.status` / `.estimate` / `.unload` are cheap bookkeeping calls.
pub(super) const FLUX2_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------
// Worker passes
// ---------------------------------------------------------------------------------------

/// Runs one FLUX.2 klein edit pass and returns the regenerated region plus what the
/// backend reports about how it got there.
///
/// `settings` must already be `normalized()`. `mask` is the L8 edit-permission mask in
/// region coordinates and must be exactly `mask_size[0] * mask_size[1]` bytes matching
/// `image.size`; `whole_region` is the mode [`mask_for_run`] derived for exactly that
/// buffer and travels with it, because the backend validates the two against each other.
/// `generation` is the progress generation claimed by `start_run`: every write into
/// `progress`, including the terminal one that always clears the bar before returning, is
/// dropped once a newer run (or a cancel) has retired it.
///
/// # Errors
/// Returns a user-facing message when the region violates the model's size contract,
/// when the mask is empty or the wrong size, when the backend fails or is unreachable,
/// when the response is missing or contradicts its declared length, or when the
/// returned PNG is not exactly the region size.
pub(super) fn run_flux2_klein(
    image: &egui::ColorImage,
    mask: &[u8],
    whole_region: bool,
    mask_size: [usize; 2],
    settings: &Flux2KleinSettings,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2RunOutcome, String> {
    let outcome = run_flux2_klein_pass(
        image,
        mask,
        whole_region,
        mask_size,
        settings,
        progress,
        generation,
    );
    // The bar is cleared on EVERY exit, including the early validation refusals above
    // the IPC call, because `start_run` raised it before the worker even started.
    update_progress(progress, generation, |state| {
        state.active = false;
        state.cancel_id = None;
    });
    outcome
}

/// The body of one run, without the progress bookkeeping [`run_flux2_klein`] wraps it
/// in. Same contract and same errors; split out only so no early return can leave the
/// bar raised.
pub(super) fn run_flux2_klein_pass(
    image: &egui::ColorImage,
    mask: &[u8],
    whole_region: bool,
    mask_size: [usize; 2],
    settings: &Flux2KleinSettings,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2RunOutcome, String> {
    if image.size != mask_size {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    let (width, height) = (image.size[0], image.size[1]);
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    // Re-checked on the worker, not only in the button gate: the gate is UI state and
    // a run can be started from a session whose region was re-derived by ratio.
    if let Some(reason) = region_block_reason(image.size) {
        return Err(reason);
    }
    // Unreachable through `mask_for_run`, which turns an empty layer into the solid mask
    // of the whole-region mode; kept because this function validates what it is HANDED
    // rather than trusting its caller, exactly as the two size checks above do, and an
    // all-zero mask would otherwise reach the backend as "change nothing".
    if !mask.iter().any(|value| *value > 0) {
        return Err(t!("cleaning.tools.flux2_klein.empty_mask_error").to_string());
    }

    let image_png = encode_color_image_png_rgba(image)?;
    let mask_png = encode_mask_png_l8(mask, width, height)?;
    let header = json!({
        "image_len": image_png.len(),
        "mask_len": mask_png.len(),
        "params": settings.to_params(whole_region),
    });
    let blob = concat_image_mask(&image_png, &mask_png);

    let stream_result = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN,
        header,
        &blob,
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |frame| publish_progress_frame(progress, generation, frame),
    );

    let (response_header, out_bytes) = stream_result?;
    if out_bytes.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    // Declared length is validated with STRICT equality before the bytes are used, so
    // a truncated or padded frame is rejected instead of decoded into garbage. The
    // field is REQUIRED: an answer without it is a protocol violation, not a licence
    // to skip the check.
    let declared = response_header
        .get("image_len")
        .and_then(Value::as_u64)
        .and_then(|declared| usize::try_from(declared).ok())
        .ok_or_else(|| t!("cleaning.inpaint.no_png_result_error").to_string())?;
    if declared != out_bytes.len() {
        return Err(tf!(
            "cleaning.tools.flux2_klein.blob_length_error",
            declared = declared,
            actual = out_bytes.len()
        ));
    }
    let out_rgba = image::load_from_memory(&out_bytes)
        .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
        .to_rgba8();
    let (out_w, out_h) = (out_rgba.width() as usize, out_rgba.height() as usize);
    if out_w != width || out_h != height {
        return Err(tf!(
            "cleaning.inpaint.unexpected_size_error",
            out_w = out_w,
            out_h = out_h,
            width = width,
            height = height
        ));
    }
    Ok(Flux2RunOutcome {
        image: egui::ColorImage::from_rgba_unmultiplied([out_w, out_h], out_rgba.as_raw()),
        oom_recovered: response_header
            .get("oom_recovered")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        applied: parse_applied_flags(&response_header),
    })
}

/// Streaming call to `method`. Each `progress` frame carries `phase`/`step`/`total`/
/// `label` in the header and no preview blob — the same shape for a generation, a
/// prompt-cache build, a component action and a model download, which is why all four go
/// through here. A download additionally fills [`Flux2ProgressFrame::file`].
///
/// `on_started` receives the IPC id of the request as soon as it is on the wire; the
/// progress state keeps it so a cancel can stop the work backend-side. This is why the call is
/// built from `begin_call` + `wait_streaming` rather than from the `call_streaming`
/// shorthand, which never exposes the id.
pub(super) fn flux2_stream_call<S, F>(
    method: &'static str,
    header: Value,
    blob: &[u8],
    on_started: S,
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    S: FnOnce(u64),
    F: FnMut(Flux2ProgressFrame),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let handle = client
        .begin_call(method, header, blob)
        .map_err(|err| map_flux2_call_error(CallError::Transport(err)))?;
    on_started(handle.id());
    handle
        .wait_streaming(
            |progress_header, _preview_blob| {
                on_progress(parse_flux2_progress_frame(progress_header));
            },
            // Per FRAME, not per call: `wait_streaming` restarts the timer on every
            // progress frame, which is what lets a multi-hour model download share this
            // timeout with a generation.
            FLUX2_RUN_TIMEOUT,
        )
        .map_err(map_flux2_call_error)
}

/// Builds the `.status` request header.
///
/// The model paths MUST travel with the query: when `params` is missing the backend
/// answers about the paths of its LAST SUCCESSFUL generation instead, which are empty
/// until one has run. An empty header therefore makes the component panel report
/// "nothing is configured" for exactly the paths the user has just entered — and the
/// panel exists to tell them whether those paths are usable.
pub(super) fn flux2_status_header(params: &Value) -> Value {
    json!({ "params": params })
}

/// Queries `.status` for the component catalog and the host's memory figures.
///
/// `params` must come from a `normalized()` settings value; empty paths are allowed
/// and are what the backend reports as "not configured".
pub(super) fn fetch_flux2_status(params: &Value) -> Result<Flux2Status, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_STATUS,
            flux2_status_header(params),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(parse_flux2_status(&header))
}

/// Parses a `.status` answer. Every field is optional: a backend that reports less
/// than the full catalog degrades to "not present" rather than to an error.
pub(super) fn parse_flux2_status(header: &Value) -> Flux2Status {
    let components = header.get("components");
    let component = |name: &str| Flux2Component::parse(components.and_then(|c| c.get(name)));
    let memory = header.get("memory");
    let memory_u64 = |name: &str| {
        memory
            .and_then(|m| m.get(name))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Flux2Status {
        available: header
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reason: header
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        text_encoder: component("text_encoder"),
        transformer: component("transformer"),
        vae: component("vae"),
        tokenizer: component("tokenizer"),
        scheduler: component("scheduler"),
        vram_total: memory_u64("vram_total"),
        ram_total: memory_u64("ram_total"),
        loaded: header
            .get("loaded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        device: header
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // Deliberately NOT defaulted to `false`: a backend that does not know about the
        // prompt cache must read as "unknown", never as "your prompt is not cached".
        prompt_cached: header.get("prompt_cached").and_then(Value::as_bool),
        // Same rule, and it matters more here: `Some(false)` gates the encode buttons and
        // raises a warning, so a backend that never reports the field must not be read as
        // "you have no encoder".
        text_encoder_available: header.get("text_encoder_available").and_then(Value::as_bool),
        // Three-state again, but the SAFE reading is inverted here: an ABSENT field means
        // supported, because that is what every backend older than the field reports and
        // what the control did before it existed. `Some(false)` is the only value that
        // closes the control — see `flux2_guidance_supported`.
        guidance_supported: header.get("guidance_supported").and_then(Value::as_bool),
        components: parse_flux2_component_snapshot(header),
    }
}

/// Reads the `components` / `components_busy` pair out of an answer header.
///
/// Shared by `.status` and `.component_action`, which report the same block — the second
/// answers with the snapshot AFTER the action, so one parser keeps the two from drifting.
///
/// An ABSENT (or non-object) `components` yields `None`, i.e. "not known". A component
/// the object does not mention simply gets no row; an `actions` entry this build does not
/// know is dropped from that row's list, and an unrecognised `residency` leaves the row's
/// state `None` with the raw literal kept. Nothing here invents a state or an action.
pub(super) fn parse_flux2_component_snapshot(header: &Value) -> Flux2ComponentSnapshot {
    let components = header
        .get("components")
        .and_then(Value::as_object)
        .map(|object| {
            Flux2ComponentId::all()
                .into_iter()
                .filter_map(|id| {
                    let entry = object.get(id.wire())?;
                    let residency_wire = entry
                        .get("residency")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let actions = entry
                        .get("actions")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .filter_map(Flux2ComponentAction::from_wire)
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Flux2ComponentResidency {
                        id,
                        residency: Flux2Residency::from_wire(&residency_wire),
                        residency_wire,
                        actions,
                    })
                })
                .collect()
        });
    Flux2ComponentSnapshot {
        components,
        components_busy: header
            .get("components_busy")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Releases the resident FLUX.2 klein pipeline on the backend.
pub(super) fn unload_flux2_klein() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
            json!({}),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(())
}

/// Builds the `.component_action` request header.
///
/// `component` and `action` sit at the TOP LEVEL beside `params`, in the same shape the
/// prompt-cache calls put their `name`/`path` beside it — `params` is the normalized
/// settings every other FLUX.2 call carries, and it is what tells the backend which model
/// paths, placement and memory flags the action is about. Without it the backend would act
/// on the paths of its last successful generation, i.e. on nothing until one has run.
///
/// `settings` must already be `normalized()`.
#[must_use]
pub(super) fn flux2_component_action_header(
    settings: &Flux2KleinSettings,
    component: Flux2ComponentId,
    action: Flux2ComponentAction,
) -> Value {
    // The mode is `false` on every path that only ASKS or MOVES something: no mask exists
    // behind a component action, exactly as behind `.status` and the prompt-cache calls.
    json!({
        "component": component.wire(),
        "action": action.wire(),
        "params": settings.to_params(false),
    })
}

/// Runs one per-component action and returns the residency snapshot that follows it.
///
/// Streaming, and it drives the SAME bar a generation and a `.prompt_cache.build` drive:
/// loading the text encoder takes tens of seconds. `generation` is the progress generation
/// claimed on the GUI thread; every write is dropped once a newer operation — or a cancel
/// — has retired it, and the bar is cleared on EVERY exit.
///
/// `header` must already carry `component`, `action` and the normalized `params`
/// (`dev-docs/flux2_component_residency.md` §4).
///
/// # Errors
/// Returns a user-facing message when the backend refuses the action (it is not in that
/// component's `actions` list, the service is busy, or the memory guard says no), when it
/// does not know the method, or when it is unreachable.
pub(super) fn run_flux2_component_action(
    header: Value,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2ComponentSnapshot, String> {
    let outcome = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION,
        header,
        &[],
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |frame| publish_progress_frame(progress, generation, frame),
    )
    .map(|(header, _blob)| parse_flux2_component_snapshot(&header));
    update_progress(progress, generation, |state| {
        state.active = false;
        state.cancel_id = None;
    });
    outcome
}

/// Translates one prompt into English through the translation tab's own dispatcher.
///
/// BLOCKING: runs only on a worker thread. `source_lang` is an MT language code
/// (`"auto"` is accepted); the target is always `"en"`, which every backend maps to
/// its own wire spelling itself.
///
/// # Errors
/// Returns the provider's message when the request fails, and a dedicated message when
/// the provider answered with no usable text at all.
pub(super) fn translate_prompt_to_english(
    service: MtService,
    source_lang: &str,
    text: String,
) -> Result<String, String> {
    let results = translate_texts_via_translator(service, source_lang, "en", vec![text])?;
    match results.into_iter().next() {
        Some(Ok(translated)) if !translated.trim().is_empty() => Ok(translated),
        Some(Ok(_)) | None => {
            Err(t!("cleaning.tools.flux2_klein.translate_empty_result_error").to_string())
        }
        Some(Err(err)) => Err(err),
    }
}

pub(super) fn map_flux2_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------

/// Concatenates the region PNG and the mask PNG into one request blob. The receiver
/// splits it by the `image_len` / `mask_len` header fields.
pub(super) fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

pub(super) fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
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

/// Encodes the L8 edit-permission mask. `mask` must be exactly `width * height` bytes.
pub(super) fn encode_mask_png_l8(mask: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
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

    #[test]
    fn blob_concat_orders_image_then_mask() {
        let image_png = b"IMAGE".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);
        assert_eq!(blob.len(), image_png.len() + mask_png.len());
        assert_eq!(&blob[..image_png.len()], image_png.as_slice());
        assert_eq!(&blob[image_png.len()..], mask_png.as_slice());
    }

    #[test]
    fn mask_encodes_one_byte_per_pixel() {
        let mask = vec![255u8; 32 * 16];
        let png = encode_mask_png_l8(&mask, 32, 16).expect("encode mask");
        let decoded = image::load_from_memory(&png).expect("decode mask").to_luma8();
        assert_eq!(decoded.dimensions(), (32, 16));
        assert!(encode_mask_png_l8(&mask, 32, 15).is_err(), "length mismatch must fail");
    }

    #[test]
    fn status_parses_both_present_spellings() {
        let header = json!({
            "available": true,
            "reason": "",
            "components": {
                "text_encoder": { "path": "/a", "exists": true, "size_bytes": 1024 },
                "tokenizer": { "found": true, "path": "/b" }
            },
            "memory": { "vram_total": 16, "ram_total": 32 },
            "loaded": true,
            "device": "cuda:0"
        });
        let status = parse_flux2_status(&header);
        assert!(status.available);
        assert!(status.text_encoder.present);
        assert_eq!(status.text_encoder.size_bytes, 1024);
        assert!(status.tokenizer.present);
        assert!(!status.vae.present, "a missing component reads as absent");
        assert_eq!(status.vram_total, 16);
        assert_eq!(status.device, "cuda:0");
    }

    #[test]
    fn the_status_reports_the_encoder_separately_from_availability() {
        // The pair this whole feature turns on: a run is available BECAUSE the prompt is
        // cached, while no encoder exists on the machine at all.
        let cached_without_encoder = parse_flux2_status(&json!({
            "available": true,
            "prompt_cached": true,
            "text_encoder_available": false
        }));
        assert!(cached_without_encoder.available);
        assert_eq!(cached_without_encoder.text_encoder_available, Some(false));
        assert_eq!(cached_without_encoder.prompt_cached, Some(true));

        let installed = parse_flux2_status(&json!({ "text_encoder_available": true }));
        assert_eq!(installed.text_encoder_available, Some(true));
        // Same three-state rule as `prompt_cached`: a backend that predates the field must
        // not be read as "you have no encoder", which would raise a false warning and close
        // the two encode buttons.
        let silent = parse_flux2_status(&json!({ "available": true }));
        assert_eq!(silent.text_encoder_available, None);
        assert_eq!(
            parse_flux2_status(&json!({ "text_encoder_available": "yes" })).text_encoder_available,
            None
        );
    }

    #[test]
    fn an_absent_guidance_flag_is_read_as_supported() {
        // The one three-state field whose SAFE default is `true`. Every backend older than
        // the field omits it, and reading that silence as "unsupported" would grey out a
        // working control for a user who only has an older backend — the failure this
        // assertion exists to catch, because it is invisible until someone runs the app.
        let silent = parse_flux2_status(&json!({ "available": true }));
        assert_eq!(silent.guidance_supported, None);
        assert!(flux2_guidance_supported(Some(&silent)));

        let distilled = parse_flux2_status(&json!({ "guidance_supported": false }));
        assert_eq!(distilled.guidance_supported, Some(false));
        assert!(!flux2_guidance_supported(Some(&distilled)));

        let regular = parse_flux2_status(&json!({ "guidance_supported": true }));
        assert_eq!(regular.guidance_supported, Some(true));
        assert!(flux2_guidance_supported(Some(&regular)));

        // A field of the wrong type is not an answer either, and must not close the control.
        let malformed = parse_flux2_status(&json!({ "guidance_supported": "no" }));
        assert_eq!(malformed.guidance_supported, None);
        assert!(flux2_guidance_supported(Some(&malformed)));
    }

    #[test]
    fn a_component_action_answer_is_read_with_the_same_parser_as_a_status() {
        // `.component_action` answers with the snapshot AFTER the action, in the same
        // shape `.status` reports it, so one parser must serve both.
        let answer = json!({
            "components_busy": false,
            "components": { "vae": { "residency": "gpu", "actions": ["unload", "warmup"] } },
        });
        assert_eq!(
            parse_flux2_component_snapshot(&answer),
            parse_flux2_status(&answer).components
        );
    }

    #[test]
    fn status_request_carries_the_model_paths() {
        let settings = Flux2KleinSettings {
            text_encoder_path: "  /models/qwen3  ".to_string(),
            transformer_path: "/models/flux2.safetensors".to_string(),
            vae_path: "/models/vae".to_string(),
            ..Flux2KleinSettings::default()
        }
        .normalized();
        let header = flux2_status_header(&settings.to_params(false));
        let params = header
            .get("params")
            .expect("`.status` without `params` makes the backend answer about the paths of the last successful run, i.e. about nothing");
        assert_eq!(params["text_encoder_path"], json!("/models/qwen3"));
        assert_eq!(
            params["transformer_path"],
            json!("/models/flux2.safetensors")
        );
        assert_eq!(params["vae_path"], json!("/models/vae"));
        // Paths the user has not filled in yet still travel, as empty strings: that is
        // exactly the question the component panel asks.
        let empty = flux2_status_header(&Flux2KleinSettings::default().normalized().to_params(false));
        assert_eq!(empty["params"]["text_encoder_path"], json!(""));
    }

    #[test]
    fn the_status_request_carries_the_prompt() {
        // `prompt_cached` is an answer ABOUT one prompt, so the prompt has to travel with
        // the question exactly as the model paths do.
        let settings = Flux2KleinSettings {
            prompt: "  remove the sfx  ".to_string(),
            ..Flux2KleinSettings::default()
        }
        .normalized();
        let header = flux2_status_header(&settings.to_params(false));
        assert_eq!(header["params"]["prompt"], json!("remove the sfx"));
    }

    #[test]
    fn the_component_action_request_names_the_component_the_action_and_the_paths() {
        let settings = runnable_settings().normalized();
        let header = flux2_component_action_header(
            &settings,
            Flux2ComponentId::TextEncoder,
            Flux2ComponentAction::Load,
        );
        assert_eq!(header.get("component").and_then(Value::as_str), Some("text_encoder"));
        assert_eq!(header.get("action").and_then(Value::as_str), Some("load"));
        let params = header.get("params").expect("the paths travel with the action");
        assert_eq!(
            params.get("text_encoder_path").and_then(Value::as_str),
            Some(settings.text_encoder_path.as_str()),
            "without them the backend would act on its last generation's paths"
        );
        assert_eq!(
            params.get("whole_region").and_then(Value::as_bool),
            Some(false),
            "no mask exists behind a component action"
        );
    }
}
