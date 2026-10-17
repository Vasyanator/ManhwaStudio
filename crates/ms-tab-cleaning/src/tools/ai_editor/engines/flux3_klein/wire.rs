/*
File: cleaning/tools/ai_editor/engines/flux3_klein/wire.rs

Purpose:
The FLUX.3 klein backend calls that are not the download: the region-edit run itself with
its request packing and answer validation, the `.status` query and `.unload`.

Main responsibilities:
- pack one run (`flux3_run_request`): header `{image_len, mask_len, reference_lens, params}`
  and the blob `region.png ++ mask.png ++ ref_0.png ++ …`, whose lengths sum EXACTLY to it;
- run it streaming on a worker (`run_flux3_klein`), publishing progress frames under the
  run's generation and clearing the bar on every exit;
- validate the answer (`parse_flux3_run_answer`): the declared `image_len` with STRICT
  equality, a PNG of exactly the region size, the echoed `seed`, the degraded steps;
- ask `.status` (`fetch_flux3_status`) and `.unload` (`unload_flux3_klein`).

Key structures:
- `Flux3RunInput`, `Flux3RunOutcome`, `Flux3Applied`

Key functions:
- `flux3_run_request()`, `run_flux3_klein()`, `parse_flux3_run_answer()`,
  `fetch_flux3_status()`, `unload_flux3_klein()`

Notes:
Every function that calls the backend runs on a WORKER thread. The wire contract is
`modules/ai_backend/ipc/PROTOCOL.md` §5.4 «FLUX.3 klein region editing» (v15); the Python
split is `_split_request_blob` in `modules/ai_backend/ipc/handlers/flux3_klein.py`, so a change
of the blob layout is a protocol change.
*/

use super::*;

/// The pixels of one run, moved onto the worker as one value.
///
/// `region`, `mask` and the optional `marks_reference` all describe `size`; `whole_region` is
/// the mode `mask_for_run` derived for exactly that `mask`. `references` are the user's extra
/// reference PNGs in order; the marks reference, when present, is sent after them.
pub(super) struct Flux3RunInput {
    pub(super) region: egui::ColorImage,
    pub(super) mask: Vec<u8>,
    pub(super) whole_region: bool,
    pub(super) size: [usize; 2],
    pub(super) references: Vec<Arc<Vec<u8>>>,
    pub(super) marks_reference: Option<egui::ColorImage>,
}

/// What the backend applied, as the answer's `applied` object reports it (validated by
/// `parse_flux3_answer_fields`). Logged, never
/// written back: every one of these is the user's choice (or the backend's resolution of
/// «auto»), not a recovery to remember.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3Applied {
    pub(super) format: String,
    pub(super) compute_path: String,
    pub(super) encoder_mode: String,
    pub(super) ae_dtype: String,
}

/// One finished run.
pub(super) struct Flux3RunOutcome {
    /// The edited region, exactly the requested size.
    pub(super) image: egui::ColorImage,
    /// The seed the run used (drawn by the backend when none was pinned).
    pub(super) seed: u64,
    /// The steps it ran.
    pub(super) steps: u64,
    pub(super) applied: Flux3Applied,
    /// Present when the backend had to degrade the step pair to finish.
    pub(super) degraded: Option<RunDegraded>,
}

/// Builds the `inpaint.flux3_klein` request: its header and the one blob it describes.
///
/// The blob is `region_png ++ mask_png ++ references[0] ++ …`; `reference_lens` lists every
/// reference's length in that order (empty = no references), so the backend can split the blob
/// with STRICT equality and refuses any request whose lengths do not sum to it.
#[must_use]
pub(super) fn flux3_run_request(region_png: &[u8], mask_png: &[u8], references: &[&[u8]], params: Value) -> (Value, Vec<u8>) {
    let total = region_png.len() + mask_png.len() + references.iter().map(|reference| reference.len()).sum::<usize>();
    let mut blob = Vec::with_capacity(total);
    blob.extend_from_slice(region_png);
    blob.extend_from_slice(mask_png);
    for reference in references {
        blob.extend_from_slice(reference);
    }
    let header = json!({
        "image_len": region_png.len(),
        "mask_len": mask_png.len(),
        "reference_lens": references.iter().map(|reference| reference.len()).collect::<Vec<_>>(),
        "params": params,
    });
    (header, blob)
}

/// Runs one region edit and returns the edited region.
///
/// `settings` must already be `normalized()`. `claim` is the progress claim taken on the GUI
/// thread by `start`: every write is dropped once a cancel retired its generation, the request
/// id goes to its control (a cancel that came first is sent the moment the id exists), and the
/// bar is cleared on EVERY exit, the early refusals included.
///
/// # Errors
/// A user-facing message: the sizes disagree, the region breaks the size rule, a PNG cannot be
/// encoded, the backend refused or is unreachable, or the answer is malformed
/// ([`parse_flux3_run_answer`]).
pub(super) fn run_flux3_klein(
    input: &Flux3RunInput,
    settings: &Flux3KleinSettings,
    progress: &Arc<Mutex<EngineProgress>>,
    claim: &ProgressClaim,
) -> Result<Flux3RunOutcome, String> {
    let outcome = run_flux3_klein_pass(input, settings, progress, claim);
    update_progress(progress, claim.generation, |state| state.active = false);
    outcome
}

/// The body of [`run_flux3_klein`] without the bar bookkeeping; split so no early return can
/// leave the bar raised.
fn run_flux3_klein_pass(
    input: &Flux3RunInput,
    settings: &Flux3KleinSettings,
    progress: &Arc<Mutex<EngineProgress>>,
    claim: &ProgressClaim,
) -> Result<Flux3RunOutcome, String> {
    let Flux3RunInput { region, mask, whole_region, size, references, marks_reference } = input;
    let [width, height] = *size;
    if region.size != *size || mask.len() != width.saturating_mul(height) || marks_reference.as_ref().is_some_and(|marks| marks.size != *size) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    // Re-checked on the worker against the one authority (`check_size`), not only by the frame.
    if let Some(reason) = region_size_refusal(width, height, &Flux3KleinEngine::frame_constraints()) {
        return Err(reason);
    }
    // `mask_for_run` never yields an all-zero mask (an empty layer becomes the solid one of
    // the whole-region mode); kept because this validates what it is handed, and the backend
    // refuses an all-zero mask with a protocol message.
    if !mask.iter().any(|value| *value > 0) {
        return Err(t!("cleaning.tools.flux3_klein.empty_mask_error").to_string());
    }
    if references.len() + usize::from(marks_reference.is_some()) > FLUX3_MAX_EXTRA_REFERENCES {
        return Err(tf!("cleaning.tools.flux3_klein.too_many_references_error", max = FLUX3_MAX_EXTRA_REFERENCES));
    }
    let region_png = encode_color_image_png_rgba(region)?;
    let mask_png = encode_mask_png_l8(mask, width, height)?;
    let marks_png = marks_reference.as_ref().map(encode_color_image_png_rgba).transpose()?;
    let mut reference_pngs: Vec<&[u8]> = references.iter().map(|png| png.as_slice()).collect();
    if let Some(marks_png) = marks_png.as_deref() {
        reference_pngs.push(marks_png);
    }
    let (header, blob) = flux3_run_request(&region_png, &mask_png, &reference_pngs, settings.to_params(*whole_region));
    let (answer, out_bytes) = stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX3_KLEIN,
        header,
        &blob,
        FLUX3_RUN_TIMEOUT,
        |id| claim.publish_request_id(id),
        |frame| publish_progress_frame(progress, claim.generation, frame),
    )?;
    parse_flux3_run_answer(&answer, &out_bytes, *size)
}

/// Validates a run answer against the region `size` it was asked for — STRICTLY, against the
/// v15 success reply (`PROTOCOL.md` §5.4; built by the backend's `region.edit_region_bytes`).
///
/// `image_len` must equal the blob length exactly and the PNG must be exactly `size` (the
/// backend never resizes). Every other field the reply is required to carry is checked for
/// presence and type: `seed`, `steps` (1..=64), `width` / `height` (= `size`), `text_tokens`,
/// `applied` (`format`, `fp8_compute` — a concrete path for fp8r, `null` otherwise —
/// `compute_path`, `encoder_mode`, `ae_dtype`, each a known value), the four step ids,
/// `degraded` and `degrade_reason` (string or `null`). A reply that misses one is a backend
/// out of step with this build, reported as such rather than as a successful run with an
/// unknown seed.
///
/// # Errors
/// A user-facing message for an empty blob, a missing or disagreeing `image_len`, an
/// undecodable PNG or one of another size, or a malformed required field (named, and logged).
pub(super) fn parse_flux3_run_answer(header: &Value, blob: &[u8], size: [usize; 2]) -> Result<Flux3RunOutcome, String> {
    if blob.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    let declared = header
        .get("image_len")
        .and_then(Value::as_u64)
        .and_then(|declared| usize::try_from(declared).ok())
        .ok_or_else(|| t!("cleaning.inpaint.no_png_result_error").to_string())?;
    if declared != blob.len() {
        return Err(tf!("cleaning.tools.flux3_klein.blob_length_error", declared = declared, actual = blob.len()));
    }
    let fields = parse_flux3_answer_fields(header, size).map_err(|field| {
        ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} run answer refused: field `{field}` is missing or malformed; header: {header}"));
        tf!("cleaning.tools.flux3_klein.malformed_answer_error", field = field)
    })?;
    let rgba = image::load_from_memory(blob).map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?.to_rgba8();
    let out = [usize::try_from(rgba.width()).unwrap_or(usize::MAX), usize::try_from(rgba.height()).unwrap_or(usize::MAX)];
    if out != size {
        return Err(tf!("cleaning.inpaint.unexpected_size_error", out_w = out[0], out_h = out[1], width = size[0], height = size[1]));
    }
    let Flux3AnswerFields { seed, steps, applied } = fields;
    Ok(Flux3RunOutcome {
        image: egui::ColorImage::from_rgba_unmultiplied(out, rgba.as_raw()),
        seed,
        steps,
        applied,
        degraded: parse_degraded(header),
    })
}

/// The required header fields of a success reply, validated.
struct Flux3AnswerFields {
    seed: u64,
    steps: u64,
    applied: Flux3Applied,
}

/// Checks every required field of a success reply (see [`parse_flux3_run_answer`]).
///
/// # Errors
/// The wire name of the first field that is missing, of the wrong type or out of its value set.
fn parse_flux3_answer_fields(header: &Value, size: [usize; 2]) -> Result<Flux3AnswerFields, &'static str> {
    let uint = |name: &'static str| header.get(name).and_then(Value::as_u64).ok_or(name);
    let seed = uint("seed")?;
    let steps = uint("steps")?;
    if !(u64::from(FLUX3_STEPS_MIN)..=u64::from(FLUX3_STEPS_MAX)).contains(&steps) {
        return Err("steps");
    }
    for (name, expected) in [("width", size[0]), ("height", size[1])] {
        if usize::try_from(uint(name)?).ok() != Some(expected) {
            return Err(name);
        }
    }
    uint("text_tokens")?;
    let applied = header.get("applied").and_then(Value::as_object).ok_or("applied")?;
    let text = |name: &'static str, allowed: &[&str]| {
        applied.get(name).and_then(Value::as_str).filter(|value| allowed.contains(value)).map(str::to_string).ok_or(name)
    };
    let format = text("format", &["bf16", "fp8r", "nvfp4"])?;
    let compute_path = text("compute_path", &["native", "scaled_mm", "dequant"])?;
    let encoder_mode = text("encoder_mode", &["text", "vl_refs"])?;
    let ae_dtype = text("ae_dtype", &["float32", "bfloat16"])?;
    // fp8r reports its concrete path, every other format `null`.
    let fp8_ok = match applied.get("fp8_compute") {
        Some(Value::Null) => format != "fp8r",
        Some(Value::String(path)) => format == "fp8r" && matches!(path.as_str(), "scaled_mm" | "dequant"),
        Some(_) | None => false,
    };
    if !fp8_ok {
        return Err("applied.fp8_compute");
    }
    let vram = |name: &'static str, effective: bool| {
        header.get(name).and_then(Value::as_str).and_then(VramStep::from_wire).filter(|step| !(effective && *step == VramStep::Auto)).ok_or(name)
    };
    let ram = |name: &'static str, effective: bool| {
        header.get(name).and_then(Value::as_str).and_then(RamStep::from_wire).filter(|step| !(effective && *step == RamStep::Auto)).ok_or(name)
    };
    vram("vram_step", false)?;
    ram("ram_step", false)?;
    vram("vram_step_effective", true)?;
    ram("ram_step_effective", true)?;
    header.get("degraded").and_then(Value::as_bool).ok_or("degraded")?;
    if !matches!(header.get("degrade_reason"), Some(Value::Null | Value::String(_))) {
        return Err("degrade_reason");
    }
    Ok(Flux3AnswerFields { seed, steps, applied: Flux3Applied { format, compute_path, encoder_mode, ae_dtype } })
}

/// Asks `.status` about `params` (normalized; their `source_root` is the package root the
/// answer describes). Worker threads only.
///
/// # Errors
/// The user-facing message of a refused or unreachable call.
pub(super) fn fetch_flux3_status(params: Value) -> Result<Flux3Status, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(backend_ipc::protocol::METHOD_INPAINT_FLUX3_KLEIN_STATUS, json!({ "params": params }), &[], FLUX3_QUERY_TIMEOUT)
        .map_err(map_call_error)?;
    Ok(parse_flux3_status(&header))
}

/// Releases the resident FLUX.3 klein weights. Returns whether anything was unloaded. Worker
/// threads only.
///
/// # Errors
/// The user-facing message of a refused or unreachable call.
pub(super) fn unload_flux3_klein() -> Result<bool, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(backend_ipc::protocol::METHOD_INPAINT_FLUX3_KLEIN_UNLOAD, json!({}), &[], FLUX3_QUERY_TIMEOUT)
        .map_err(map_call_error)?;
    Ok(header.get("unloaded").and_then(Value::as_bool).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared lengths cover the blob exactly and in order — the backend splits it with
    /// strict equality.
    #[test]
    fn the_request_names_every_segment_in_order() {
        let (header, blob) = flux3_run_request(b"REGION", b"MASK", &[b"R1", b"REF2"], json!({ "steps": 16 }));
        assert_eq!(blob, b"REGIONMASKR1REF2");
        assert_eq!(header["image_len"], json!(6));
        assert_eq!(header["mask_len"], json!(4));
        assert_eq!(header["reference_lens"], json!([2, 4]));
        assert_eq!(header["params"]["steps"], json!(16));
        let declared: u64 = header["image_len"].as_u64().unwrap_or(0)
            + header["mask_len"].as_u64().unwrap_or(0)
            + header["reference_lens"].as_array().map_or(0, |lens| lens.iter().filter_map(Value::as_u64).sum());
        assert_eq!(usize::try_from(declared).ok(), Some(blob.len()));
        let (bare, _) = flux3_run_request(b"R", b"M", &[], json!({}));
        assert_eq!(bare["reference_lens"], json!([]), "no references is an empty list");
    }

    fn png(width: usize, height: usize) -> Vec<u8> {
        let image = egui::ColorImage::filled([width, height], Color32::from_rgb(1, 2, 3));
        encode_color_image_png_rgba(&image).expect("encode")
    }

    /// A complete v15 success reply for a `w`x`h` region (`region.edit_region_bytes`).
    fn full_answer(blob: &[u8], w: usize, h: usize) -> Value {
        json!({
            "image_len": blob.len(),
            "seed": 3_141_592_653u64,
            "steps": 16,
            "width": w,
            "height": h,
            "text_tokens": 77,
            "applied": { "format": "fp8r", "fp8_compute": "dequant", "compute_path": "dequant", "encoder_mode": "text", "ae_dtype": "float32" },
            "vram_step": "auto", "ram_step": "auto", "vram_step_effective": "half", "ram_step_effective": "run",
            "degraded": true, "degrade_reason": "device_oom"
        })
    }

    /// The answer shape of the backend: the region PNG, its length, the echoed seed, the
    /// applied object and the §5.10 degrade keys.
    #[test]
    fn a_run_answer_is_validated_and_its_seed_read() {
        let blob = png(128, 256);
        let outcome = parse_flux3_run_answer(&full_answer(&blob, 128, 256), &blob, [128, 256]).expect("a valid answer");
        assert_eq!(outcome.image.size, [128, 256]);
        assert_eq!(outcome.seed, 3_141_592_653);
        assert_eq!(outcome.steps, 16);
        assert_eq!(outcome.applied.compute_path, "dequant");
        let degraded = outcome.degraded.expect("degraded");
        assert_eq!(degraded.vram, Some(VramStep::Half));
        assert_eq!(degraded.ram, Some(RamStep::Run));
        // A bf16 run reports no fp8 path, and an undegraded one a null reason.
        let mut bf16 = full_answer(&blob, 128, 256);
        bf16["applied"]["format"] = json!("bf16");
        bf16["applied"]["fp8_compute"] = Value::Null;
        bf16["applied"]["compute_path"] = json!("native");
        bf16["degraded"] = json!(false);
        bf16["degrade_reason"] = Value::Null;
        let outcome = parse_flux3_run_answer(&bf16, &blob, [128, 256]).expect("a valid bf16 answer");
        assert!(outcome.degraded.is_none());
    }

    /// The length, the blob and the size are refused as before.
    #[test]
    fn a_malformed_answer_is_refused() {
        let blob = png(128, 128);
        let mut wrong_len = full_answer(&blob, 128, 128);
        wrong_len["image_len"] = json!(blob.len() + 1);
        assert!(parse_flux3_run_answer(&wrong_len, &blob, [128, 128]).is_err());
        let mut no_len = full_answer(&blob, 128, 128);
        if let Some(object) = no_len.as_object_mut() {
            object.remove("image_len");
        }
        assert!(parse_flux3_run_answer(&no_len, &blob, [128, 128]).is_err(), "image_len is required");
        assert!(parse_flux3_run_answer(&full_answer(&[], 128, 128), &[], [128, 128]).is_err());
        let resized = png(128, 144);
        assert!(parse_flux3_run_answer(&full_answer(&resized, 128, 128), &resized, [128, 128]).is_err(), "the backend never resizes");
    }

    /// Every required field, missing and wrong-typed (or out of its value set), is refused and
    /// named — never a "successful" run with an unknown seed.
    #[test]
    fn every_required_answer_field_is_checked() {
        let blob = png(128, 128);
        let cases: Vec<(&str, Option<Value>, &str)> = vec![
            ("seed", None, "seed"),
            ("seed", Some(json!("42")), "seed"),
            ("seed", Some(json!(-1)), "seed"),
            ("steps", None, "steps"),
            ("steps", Some(json!(1.5)), "steps"),
            ("steps", Some(json!(0)), "steps"),
            ("width", None, "width"),
            ("width", Some(json!(256)), "width"),
            ("height", Some(json!("128")), "height"),
            ("text_tokens", None, "text_tokens"),
            ("text_tokens", Some(json!(true)), "text_tokens"),
            ("applied", None, "applied"),
            ("applied", Some(json!("fp8r")), "applied"),
            ("vram_step", None, "vram_step"),
            ("ram_step", Some(json!(3)), "ram_step"),
            ("vram_step_effective", Some(json!("auto")), "vram_step_effective"),
            ("ram_step_effective", None, "ram_step_effective"),
            ("degraded", None, "degraded"),
            ("degraded", Some(json!("yes")), "degraded"),
            ("degrade_reason", None, "degrade_reason"),
            ("degrade_reason", Some(json!(7)), "degrade_reason"),
        ];
        for (field, value, named) in cases {
            let mut answer = full_answer(&blob, 128, 128);
            match value {
                Some(value) => answer[field] = value,
                None => {
                    if let Some(object) = answer.as_object_mut() {
                        object.remove(field);
                    }
                }
            }
            assert_eq!(parse_flux3_answer_fields(&answer, [128, 128]).err(), Some(named), "{field}");
            assert!(parse_flux3_run_answer(&answer, &blob, [128, 128]).is_err(), "{field}");
        }
        let applied_cases: Vec<(&str, Option<Value>, &str)> = vec![
            ("format", None, "format"),
            ("format", Some(json!("int8")), "format"),
            ("compute_path", Some(json!(1)), "compute_path"),
            ("compute_path", Some(json!("magic")), "compute_path"),
            ("encoder_mode", None, "encoder_mode"),
            ("ae_dtype", Some(json!("float16")), "ae_dtype"),
            ("fp8_compute", None, "applied.fp8_compute"),
            ("fp8_compute", Some(Value::Null), "applied.fp8_compute"),
            ("fp8_compute", Some(json!("auto")), "applied.fp8_compute"),
        ];
        for (field, value, named) in applied_cases {
            let mut answer = full_answer(&blob, 128, 128);
            match value {
                Some(value) => answer["applied"][field] = value,
                None => {
                    if let Some(object) = answer["applied"].as_object_mut() {
                        object.remove(field);
                    }
                }
            }
            assert_eq!(parse_flux3_answer_fields(&answer, [128, 128]).err(), Some(named), "applied.{field}");
        }
        // A non-fp8r format must not report an fp8 path.
        let mut nvfp4 = full_answer(&blob, 128, 128);
        nvfp4["applied"]["format"] = json!("nvfp4");
        assert_eq!(parse_flux3_answer_fields(&nvfp4, [128, 128]).err(), Some("applied.fp8_compute"));
    }

    /// The worker refuses what the backend would, before encoding anything — no backend needed.
    #[test]
    fn the_worker_refuses_a_bad_region_before_the_wire() {
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let settings = Flux3KleinSettings::default().normalized();
        let input = |size: [usize; 2], references: usize| Flux3RunInput {
            region: egui::ColorImage::filled(size, Color32::WHITE),
            mask: vec![255; size[0] * size[1]],
            whole_region: true,
            size,
            references: (0..references).map(|_| Arc::new(vec![1u8])).collect(),
            marks_reference: None,
        };
        let claim = begin_progress_generation(&progress);
        assert!(run_flux3_klein(&input([120, 128], 0), &settings, &progress, &claim).is_err(), "off the 16 grid");
        assert!(run_flux3_klein(&input([128, 1024], 0), &settings, &progress, &claim).is_err(), "steeper than 4:1");
        assert!(run_flux3_klein(&input([128, 128], 10), &settings, &progress, &claim).is_err(), "ten extra references");
        assert!(!lock_progress(&progress).active, "the bar is cleared on a refusal");
    }
}
