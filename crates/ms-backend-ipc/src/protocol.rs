/*
File: crates/ms-backend-ipc/src/protocol.rs

Purpose:
Rust mirror of the framed-protocol constants in `modules/ai_backend/ipc/protocol.py`.
This is the single Rust-side source of truth for the wire contract: protocol
version, frame kinds, response statuses, event topics, method names, and header
field names. Every string/number value here MUST match `protocol.py`
byte-for-byte, since both sides are implemented purely from `PROTOCOL.md`.

Notes:
Constants only — no logic, no socket code, no frame codec (that lives in the
`frame` module). The header builder helpers at the bottom are convenience wrappers
around `serde_json` and carry no protocol state.
*/

// The protocol constant table mirrors the full Python contract. Not every
// constant is exercised by every call site, so the as-yet-unused constants are
// intentional.
#![allow(dead_code)]

use serde_json::{Map, Value, json};

// ============================================================================
// PROTOCOL VERSION
// ============================================================================

/// Protocol version compared during the `hello` handshake. Mirrors
/// `PROTOCOL_VERSION` in `modules/ai_backend/ipc/protocol.py`.
///
/// This constant is the ONLY compatibility gate between the Rust application and the
/// Python backend: `client::verify_hello` rejects a connection whose `hello` reply
/// carries a different value. The program version (`MS_APP_VERSION` /
/// `config.VERSION`) is never compared — it is diagnostic information only.
///
/// It MUST be bumped in BOTH files together on ANY change to that contract, not only on
/// one judged breaking: a new method, a new header or payload field, a new topic, a
/// changed meaning of an existing field, a changed blob format. Deciding whether a change
/// "really" breaks anything is exactly the judgement that gets made wrong, and bumping
/// costs nothing because both halves ship and update together.
/// `python_protocol_version_matches_rust` below guards the mirror — but nothing can detect
/// a bump that was never made: a client and a backend from different builds then agree on
/// a contract that does not exist and fail at runtime instead of being refused in `hello`.
///
/// The contract also covers state the two processes SHARE on disk: the `user_config`
/// document's storage semantics (`ms-docstore` / `docstore.py`, `.json` or SQLite `.db`).
/// A backend payload that reads/writes it differently must be refused here.
///
/// 9: `General.android_native_windows` replaced by `General.secondary_windows` (migrated on load).
/// 10: new `General.touch_gestures` boolean (canvas finger gestures; per-platform default).
/// 11: `Hotkeys` section format 2 (`{"format": 2, "actions": {id: [alternative, ...]}}`, written
/// by `ms-input`; a version-1 section is migrated and rewritten on studio open).
/// 12: EasyOCR / Surya / SDXL load offline from Rust-downloaded directories: new required
/// fields [`HEADER_EASYOCR_MODEL_DIR`] (`ocr.easy`), [`HEADER_SURYA_RECOGNITION_DIR`] +
/// [`HEADER_SURYA_DETECTION_DIR`] (`ocr.surya`), [`HEADER_SURYA_DETECTION_DIR`]
/// (`textdetector.surya.forward`), [`HEADER_SDXL_CONFIG_DIR`] (`inpaint.sdxl`), plus
/// [`HEADER_SDXL_MODEL_DIR`] (`inpaint.sdxl`, required for the curated repo id);
/// [`HEADER_EASY_LANGS`] is no longer aliased by the backend; other SDXL repo ids are refused.
/// 13: large-model pool: new methods [`METHOD_LARGE_MODELS_PROFILES`],
/// [`METHOD_LARGE_MODELS_LIMITS_SET`], [`METHOD_LARGE_MODELS_STATUS`],
/// [`METHOD_LARGE_MODELS_UNLOAD`], [`METHOD_LARGE_MODELS_LEARNED_RESET`]; the `health` payload's
/// `model_manager` object gains `small_models`, `large_models`, `limits`, `budget`; new
/// `user_config` keys `General.large_models_{vram_cap_bytes, ram_cap_bytes}` (written by the
/// backend) and `General.large_models_oom_score_adj` (read at backend start). Residency is chosen
/// on two step axes, `vram_step` x `ram_step`; `inpaint.flux2_klein` gains the optional step
/// fields additively.
/// 14: the FLUX.2 klein placement wire is retired: `inpaint.flux2_klein*` params refuse
/// `placement`, `profile`, `low_cpu_mem_usage`, `vae_slicing`, `unload_transformer_before_vae`,
/// `unload_text_encoder_after_encode` by name; `applied` is `{vae_tiling}`; the run answer loses
/// `placement`, `profile`, `profile_effective`; `.status` loses `memory`; the method
/// `inpaint.flux2_klein.estimate` is removed (forecasts are [`METHOD_LARGE_MODELS_PROFILES`]);
/// the text encoder is never pinned to the CPU and the `….cpu` encoder registrations are gone;
/// new `user_config` default `General.large_models_oom_score_adj: true`.
/// 15: FLUX.3 klein region editing: new methods [`METHOD_INPAINT_FLUX3_KLEIN`] (streaming, blob
/// region ++ mask ++ references, frames with a stable `stage`),
/// [`METHOD_INPAINT_FLUX3_KLEIN_STATUS`], [`METHOD_INPAINT_FLUX3_KLEIN_UNLOAD`],
/// [`METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK`], [`METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START`];
/// [`METHOD_LARGE_MODELS_PROFILES`] serves `model: "flux3_klein"`; the `health` payload gains
/// `inpaint.flux3_klein`; «Энкодер на CPU»: optional `encoder_on_cpu` (bool, absent = true) in
/// the `inpaint.flux2_klein*` params, the FLUX.3 params and their `large_models.profiles` params
/// (the text encoder then runs on the CPU at every VRAM step).
/// 16: local colour match in the FLUX.2 klein and FLUX.3 klein region editors: optional
/// `color_match_local_pct` (integer 0..=100, absent = 60; 0 = global match only) and
/// `color_match_scale_px` (integer 8..=96, absent = 24) in the `inpaint.flux2_klein*` and
/// `inpaint.flux3_klein*` params (the low-frequency Lab mean/contrast match inside the mask,
/// applied after the global `color_match` and only under it).
/// 17: the FLUX.2 klein and FLUX.3 klein region post-processing is the client's: both
/// answers are the RAW generated region (RGB PNG; no colour match, no composite, so outside
/// the mask the FLUX.2 answer is a VAE round trip); the `inpaint.flux2_klein` mask is the
/// latent mask, already dilated by the client, and its params refuse `mask_dilate_px`,
/// `mask_feather_px`, `color_match`, `color_match_local_pct`, `color_match_scale_px` by name;
/// those four post fields are unknown keys of the `inpaint.flux3_klein` params (refused), whose
/// mask is validated only; the FLUX.3 decode rounds to `u8` instead of truncating.
/// 18: the `health` payload gains `torch` (`null`, or `{flavour, version, hip_version,
/// cuda_version, devices: [{index, name, gcn_arch, integrated}], rocm_device_archs,
/// rocm_split_kernels}`: the backend's PyTorch build and visible GPUs; absent from the
/// warming-up form); new
/// `user_config` keys `General.torch_flavour` / `General.torch_rocm_gfx` (the installer's
/// recorded torch choice, written by the installer, never read by the backend).
/// 19: `user_config` `Canvas.{aside_compact_mode, aside_side_mode, aside_second_column,
/// scale_bubbles, aside_min_width_px, aside_max_width_px, spellcheck_original,
/// spellcheck_translation}` are no longer written (the canvas layout is per title; the six layout
/// keys are read only as the migration source of titles without `canvas.layout_per_title`; the
/// two spellcheck copies were never read); new keys `Canvas.layout_presets` (the app-wide layout
/// preset store, written by the Settings pane) and `TranslarionTab.composition_presets` (the
/// composition panel's app-wide presets, written by the Translation tab). The backend reads none
/// of them.
/// 20: the `model_load` event topic is removed (it was declared but never published or
/// subscribed to); the valid topics are `health`, `device`, `log`.
/// 21: FLUX.2 klein weight formats: the `inpaint.flux2_klein*` params and their
/// `large_models.profiles` params gain optional `transformer_format` / `transformer_formats` and
/// `text_encoder_format` / `text_encoder_formats` (selected id + candidates `[{id, path}]`, best
/// first, ids `^[a-z0-9_]{1,32}$` opaque to the backend; a present list must hold the selected id
/// with `path` equal to that component's `*_path`; absent, the id is derived from the container:
/// `bf16` for safetensors or a folder, `gguf` for a GGUF file); `transformer_path` and
/// `text_encoder_path` may name a `.gguf` file (`text_encoder_fp8: true` with a GGUF encoder is
/// refused); `inpaint.flux2_klein.download.*` refuses `variant: "4b"` (the app's model catalog
/// downloads 4B); the FLUX.2 `large_models.profiles` answer gains additive `encoder_formats`, and
/// its `encoder: null` now means "not needed AND not resident" (a resident encoder is counted
/// even when the prompt is cached; `selected` stays the pipeline's plan, the client sums the
/// two; each of the two `forecast` objects gains additive `host_baseline`, the baseline RSS its
/// `host_peak` includes, so the sum counts the process once); FLUX.2 `phase:"load"` frames carry
/// the additive byte level `file_step` / `file_total` / `file_label` for the text encoder too (on
/// step 7, `LOAD_STEP_TEXT_ENCODER`) and for a transformer folder (on step 1; `file_label` = the
/// file or folder name; a read inside a later phase rides on the step current then, a `generate`
/// frame during the denoise), plus additive `file_component` (`"transformer"` /
/// `"text_encoder"`; absent = transformer).
/// FLUX.3 klein: every checkpoint read of `inpaint.flux3_klein` adds the same
/// byte level to the `phase:"load"` frame of the current stage (`stage:"denoise"`
/// for a reload during the denoise): `file_step` / `file_total` (bytes of this
/// read), `file_label` (file name; folder name when sharded) and
/// `file_component` (`transformer` = DiT, `text_encoder`, `vae` = AE).
/// Load byte frames of both klein models: at most one per 0.2 s per read, plus
/// one 100% frame.
pub const PROTOCOL_VERSION: u32 = 21;

// ============================================================================
// FRAME SIZE GUARDS
// ============================================================================

/// Hard upper bound on the `header_json` segment (1 MiB). Mirrors
/// `protocol.MAX_HEADER_BYTES`.
pub const MAX_HEADER_BYTES: usize = 1024 * 1024;

/// Hard upper bound on the binary blob segment (32 MiB). Mirrors
/// `protocol.MAX_BLOB_BYTES`.
pub const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;

// ============================================================================
// FRAME KINDS (`kind` header field)
// ============================================================================

pub const KIND_HELLO: &str = "hello";
pub const KIND_REQUEST: &str = "request";
pub const KIND_RESPONSE: &str = "response";
pub const KIND_PROGRESS: &str = "progress";
pub const KIND_EVENT: &str = "event";
pub const KIND_CANCEL: &str = "cancel";
pub const KIND_ERROR: &str = "error";

// ============================================================================
// RESPONSE STATUS VALUES (`status` header field on `response`)
// ============================================================================

pub const STATUS_OK: &str = "ok";
pub const STATUS_ERROR: &str = "error";
pub const STATUS_INTERRUPTED: &str = "interrupted";

// ============================================================================
// EVENT TOPICS (`topic` header field on `event`, id=0)
// ============================================================================

pub const TOPIC_HEALTH: &str = "health";
pub const TOPIC_DEVICE: &str = "device";
pub const TOPIC_LOG: &str = "log";

// ============================================================================
// METHOD NAMES (dotted namespace.action form)
// ============================================================================

// --- OCR ---
pub const METHOD_OCR_MANGA: &str = "ocr.manga";
pub const METHOD_OCR_EASY: &str = "ocr.easy";
pub const METHOD_OCR_PADDLE: &str = "ocr.paddle";
/// PaddleOCR-VL through the backend's vendored model code. Since protocol v7 the request
/// header MUST carry `paddle_vl_model` (the variant label, `^[a-z0-9_]{1,64}$`, used for the
/// lease key, logs and the health `ocr.paddleocrvl.model` echo) and `paddle_vl_model_dir`
/// (absolute directory of the downloaded variant); the backend never downloads it.
pub const METHOD_OCR_PADDLE_VL: &str = "ocr.paddle_vl";
pub const METHOD_OCR_SURYA: &str = "ocr.surya";
pub const METHOD_OCR_PADDLE_ONNX: &str = "ocr.paddle_onnx";
/// Baberu OCR on the backend's ONNX Runtime (protocol v7; vision on the selected ONNX
/// provider, decoders on the CPU provider): the fallback of the native engine. Request
/// header: `baberu_model_files` = `{vision, prefill, step, vocab}`, each an absolute path
/// of the downloaded model; plus `join_newlines` and `reflect_strings`. Answers `engine: "baberuocr"`, `lines`, `text`.
pub const METHOD_OCR_BABERU: &str = "ocr.baberu";

// --- Machine translation ---
pub const METHOD_TRANSLATE_DEEP: &str = "translate.deep";

// --- Inpaint ---
pub const METHOD_INPAINT_LAMA_V2: &str = "inpaint.lama_v2";
pub const METHOD_INPAINT_LAMA_V2_UNLOAD: &str = "inpaint.lama_v2.unload";
pub const METHOD_INPAINT_LAMA_MPE: &str = "inpaint.lama_mpe";
pub const METHOD_INPAINT_LAMA_MPE_UNLOAD: &str = "inpaint.lama_mpe.unload";
pub const METHOD_INPAINT_AOT: &str = "inpaint.aot";
pub const METHOD_INPAINT_AOT_UNLOAD: &str = "inpaint.aot.unload";
pub const METHOD_INPAINT_SDXL: &str = "inpaint.sdxl";
pub const METHOD_INPAINT_SDXL_UNLOAD: &str = "inpaint.sdxl.unload";
pub const METHOD_INPAINT_FLUX_FILL: &str = "inpaint.flux_fill";
pub const METHOD_INPAINT_FLUX_FILL_UNLOAD: &str = "inpaint.flux_fill.unload";
pub const METHOD_INPAINT_FLUX_FILL_STATUS: &str = "inpaint.flux_fill.status";
/// FLUX.2 klein region edit (streaming). Request header carries `image_len`,
/// `mask_len`, `params` and, only when a marks reference travels, `reference_len`; the
/// blob is `region.png ++ mask.png [++ reference.png]` (mask L8, reference RGB(A), both
/// exactly the region size; the lengths must sum to the blob exactly, and an absent or
/// zero `reference_len` means no reference). The response header carries `image_len` and the blob is the
/// edited region as an RGB PNG of exactly the region size.
pub const METHOD_INPAINT_FLUX2_KLEIN: &str = "inpaint.flux2_klein";
/// Reports whether the FLUX.2 klein components resolve on disk and where each one is
/// resident (free memory and forecasts are [`METHOD_LARGE_MODELS_PROFILES`]).
pub const METHOD_INPAINT_FLUX2_KLEIN_STATUS: &str = "inpaint.flux2_klein.status";
/// Releases the resident FLUX.2 klein pipeline.
pub const METHOD_INPAINT_FLUX2_KLEIN_UNLOAD: &str = "inpaint.flux2_klein.unload";
/// Loads, unloads or moves ONE FLUX.2 klein component (text encoder, transformer,
/// VAE). Streaming, like `.prompt_cache.build`, because reading the ~16 GB text
/// encoder takes tens of seconds; it therefore claims the same single progress bar and can
/// never run beside a generation.
///
/// Request header: `component` (`text_encoder` / `transformer` / `vae`), `action`
/// (`load` / `unload` / `to_ram` / `to_gpu` / `warmup`) and the normalized `params`
/// every other FLUX.2 call carries. The response repeats the `.status` `components`
/// block as it stands AFTER the action, plus `components_busy`.
///
/// An action the component's own `actions` list does not offer, a service already
/// busy, and a memory guard refusal are all ERRORS with an actionable message —
/// never a silent no-op. The wire contract is `dev-docs/flux2_component_residency.md`.
pub const METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION: &str =
    "inpaint.flux2_klein.component_action";
/// Encodes the `params.prompt` with the Qwen3 text encoder and keeps the
/// embeddings in the backend's prompt cache (streaming: reading the ~16 GB encoder
/// takes tens of seconds, and the progress frames carry `phase`/`step`/`total`/`label` just
/// like a generation). Afterwards `.status` reports `prompt_cached = true` for that
/// prompt and a generation skips the encoder entirely.
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD: &str =
    "inpaint.flux2_klein.prompt_cache.build";
/// Lists the saved prompt-cache LIBRARY entries of the encoder family named by
/// `params`. The answer carries the family name plus one record per entry (name,
/// the prompt it was built from, and when it was created).
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST: &str =
    "inpaint.flux2_klein.prompt_cache.list";
/// Stores the cached embeddings of `params.prompt` in the library under
/// `params.name`. A name already taken is refused with an explicit error rather
/// than overwritten. One-shot.
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE: &str =
    "inpaint.flux2_klein.prompt_cache.save";
/// Loads library entry `params.name` into the backend's live cache and answers with
/// the `prompt` it was built from, so the tool can show what was actually loaded. An
/// entry belonging to a different encoder family is refused. One-shot.
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD: &str =
    "inpaint.flux2_klein.prompt_cache.load";
/// Writes library entry `params.name` to the file `params.path`, for handing it to
/// someone else. One-shot.
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT: &str =
    "inpaint.flux2_klein.prompt_cache.export";
/// Takes the file `params.path` into the library. A file built for a DIFFERENT
/// encoder family is still imported — into that family's folder, so it is not lost —
/// and the answer says so; such an entry does not appear in the current family's
/// listing and cannot be loaded. One-shot.
pub const METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT: &str =
    "inpaint.flux2_klein.prompt_cache.import";
/// Checks Hugging Face access to the FLUX.2 klein repositories and prices the
/// download, without transferring anything. One-shot.
///
/// Request header: `hf_token` (a string, possibly empty), `uncensored` (bool —
/// whether the uncensored text-encoder repository is needed as well) and `variant`
/// (`"9b"`, the default when absent, or `"4b"` — which checkpoint is meant). The
/// token is NEVER logged on either side. `"4b"` together with `uncensored = true` is
/// refused: no uncensored encoder is published for that checkpoint.
///
/// The answer carries `repos`, one entry per repository the toggle actually needs,
/// each `{ "state": … }` where the state is one of `ok` / `no_token` /
/// `invalid_token` / `not_accepted` / `not_found` / `network_error`, plus `plan`
/// with `total_bytes`, `missing_bytes` and `missing_files` computed from the same
/// listing, plus `variant` ECHOED back so a stale answer about the other checkpoint
/// is detectable. The pinned wire contract is `dev-docs/flux2_model_download.md` §3.
pub const METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK: &str = "inpaint.flux2_klein.download.check";
/// Downloads the FLUX.2 klein model files into the requested variant's own directory
/// (`side_models/FLUX.2-klein-9B/` or `side_models/FLUX.2-klein-4B/`).
///
/// STREAMING, the same envelope as `.prompt_cache.build`, so it claims the same
/// single progress bar and can never run beside a generation. Request header:
/// `hf_token`, `uncensored` and `variant`, as for `.download.check`.
///
/// Progress frames keep the usual `phase` / `step` / `total` / `label` — where
/// `step` and `total` are OVERALL BYTES across the whole plan, which keeps every
/// existing single-level consumer correct — and ADD three OPTIONAL fields for the
/// file in flight: `file_step`, `file_total` and `file_label`. A frame that omits
/// them (a preparation phase) is legal and renders as the overall bar alone.
///
/// The answer carries `paths` (`transformer` / `text_encoder` / `vae`),
/// `downloaded_bytes` and `skipped_files`; the tool writes those three paths into
/// its settings, so a finished download leaves a configured engine. The pinned wire
/// contract is `dev-docs/flux2_model_download.md` §4.
pub const METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START: &str = "inpaint.flux2_klein.download.start";
/// FLUX.3 klein region edit (streaming, mid-run cancel). Request header: `image_len`,
/// `mask_len`, `reference_lens` (0..9 integers) and `params`; the blob is
/// `region.png ++ mask.png ++ reference_0.png ++ …` (mask L8 of the region size; the lengths
/// must sum to the blob exactly). The region is reference #0 and the result is composited
/// inside the mask. The response header carries `image_len`, `seed`, `steps`, `width`,
/// `height`, `text_tokens`, `applied` and the step keys; the blob is the edited region as an
/// RGB PNG of exactly the region size. Progress frames carry `phase` (`load` / `generate`),
/// `step`, `total`, `label` and a stable `stage` id. Wire: `PROTOCOL.md` "FLUX.3 klein".
pub const METHOD_INPAINT_FLUX3_KLEIN: &str = "inpaint.flux3_klein";
/// Reports the FLUX.3 klein package files under `params.source_root` per format and the
/// service health (free memory and forecasts are [`METHOD_LARGE_MODELS_PROFILES`]).
pub const METHOD_INPAINT_FLUX3_KLEIN_STATUS: &str = "inpaint.flux3_klein.status";
/// Releases every idle FLUX.3 klein handle.
pub const METHOD_INPAINT_FLUX3_KLEIN_UNLOAD: &str = "inpaint.flux3_klein.unload";
/// Checks Hugging Face access to the gated FLUX.3 klein repository and prices the download of
/// one weight format, without transferring anything. Request header: `hf_token`, `format`
/// (`bf16` / `fp8r` / `nvfp4`, default `fp8r`). The answer: `format` (echo), `repos`
/// (one `{state, message}`), `plan` (nullable) and `plan_error`, as for
/// [`METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK`].
pub const METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK: &str = "inpaint.flux3_klein.download.check";
/// Downloads one weight format of the FLUX.3 klein package into `side_models/FLUX.3-image-klein/`
/// (streaming two-level byte progress, cancel inside the transfer). Request header: `hf_token`,
/// `format`. The answer: `root` (the package root), `downloaded_bytes`, `skipped_files`.
pub const METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START: &str = "inpaint.flux3_klein.download.start";

// --- Visible watermark removal ---
/// Predicts a watermark mask for the request blob; responds with an L8 mask PNG
/// at the input resolution.
pub const METHOD_WATERMARK_DETECT: &str = "watermark.detect";
/// Direct network pass (experimental): responds with the cleaned image PNG
/// followed by the predicted mask PNG in one blob, delimited by `*_len` headers.
pub const METHOD_WATERMARK_REMOVE: &str = "watermark.remove";
/// Reports the model catalog: available models, the ones already downloaded, and
/// the default choice.
pub const METHOD_WATERMARK_STATUS: &str = "watermark.status";
/// Releases the resident watermark model; responds with whether one was loaded.
pub const METHOD_WATERMARK_UNLOAD: &str = "watermark.unload";

// --- Text detection (forward-only since protocol v4) ---
/// CTD forward pass: equal-size RGB tiles in, `[seg, shrink]` u8 maps out. The wire
/// contract (header fields, blob layout, validation) is `crate::textdetector`.
pub const METHOD_TEXTDETECTOR_CTD_FORWARD: &str = "textdetector.ctd.forward";
/// PP-OCR detection forward pass: one `prob` u8 map per tile at the tile resolution.
pub const METHOD_TEXTDETECTOR_PADDLE_FORWARD: &str = "textdetector.paddle.forward";
/// Surya forward pass: one `text` u8 map per tile at a quarter of the tile resolution.
pub const METHOD_TEXTDETECTOR_SURYA_FORWARD: &str = "textdetector.surya.forward";

// --- Device ---
pub const METHOD_DEVICE_GET: &str = "device.get";
pub const METHOD_DEVICE_SET: &str = "device.set";
pub const METHOD_DEVICE_CUDA_DIAGNOSTICS: &str = "device.cuda_diagnostics";

// --- Reline ---
pub const METHOD_RELINE_MODELS: &str = "reline.models";
pub const METHOD_RELINE_PROCESS: &str = "reline.process";

// --- Browser scraping (Selenium / CloakBrowser) ---
// Carries a legacy advanced-download command object in the request header
// `payload` field; progress streams as `progress` frames and the daemon's
// terminal event dict is the response header. Mirrors Python METHOD_BROWSER_COMMAND.
pub const METHOD_BROWSER_COMMAND: &str = "browser.command";

// --- Large-model pool (protocol v13) ---
/// Plans the VRAM / RAM step pickers of one large model for one request shape (read-only: the
/// backend probes and plans, never loads). Request: `model`, `params`, `shape`, optional
/// `format` / `vram_step` / `ram_step`; answer: `selected`, `vram`, `ram`, `learned_floor`,
/// `limits`, `formats`, `suggestions`.
/// Shapes: `modules/ai_backend/ipc/PROTOCOL.md` §5.10.
pub const METHOD_LARGE_MODELS_PROFILES: &str = "large_models.profiles";
/// Sets the large-model pool's hard VRAM / RAM caps (persisted by the backend in
/// `user_config` `General`); answers the limits in force.
pub const METHOD_LARGE_MODELS_LIMITS_SET: &str = "large_models.limits.set";
/// Loaded large-model handles (effective steps, per-component placement) and the limits in force.
pub const METHOD_LARGE_MODELS_STATUS: &str = "large_models.status";
/// Drops the idle large-model handles (all, or one model's); answers the dropped keys.
pub const METHOD_LARGE_MODELS_UNLOAD: &str = "large_models.unload";
/// Forgets the learned step floors (all, or one model's); answers how many were cleared.
pub const METHOD_LARGE_MODELS_LEARNED_RESET: &str = "large_models.learned.reset";

// --- Health ---
pub const METHOD_HEALTH: &str = "health";

// ============================================================================
// HEADER FIELD NAMES (canonical keys inside `header_json`)
// ============================================================================

pub const HEADER_VERSION: &str = "v";
pub const HEADER_ID: &str = "id";
pub const HEADER_KIND: &str = "kind";
pub const HEADER_METHOD: &str = "method";
pub const HEADER_TOPIC: &str = "topic";
pub const HEADER_STATUS: &str = "status";
pub const HEADER_ERROR: &str = "error";
pub const HEADER_BACKEND_VERSION: &str = "backend_version";

// --- Model locations sent by Rust (the backend never downloads these models) ---
/// `ocr.easy` (v12, required): absolute directory holding the EasyOCR detector
/// `craft_mlt_25k.pth` and every downloaded recognizer (one shared directory).
pub const HEADER_EASYOCR_MODEL_DIR: &str = "easyocr_model_dir";
/// `ocr.easy`: canonical EasyOCR language codes, comma-separated; since v12 the backend uses
/// them verbatim (Rust owns normalization and the script choice).
pub const HEADER_EASY_LANGS: &str = "easy_langs";
/// `ocr.surya` (v12, required): absolute `text_recognition` checkpoint directory (foundation +
/// recognition, Surya's `manifest.json` layout).
pub const HEADER_SURYA_RECOGNITION_DIR: &str = "surya_recognition_dir";
/// `ocr.surya` and `textdetector.surya.forward` (v12, required): absolute `text_detection`
/// checkpoint directory.
pub const HEADER_SURYA_DETECTION_DIR: &str = "surya_detection_dir";
/// `inpaint.sdxl` (v12, required): absolute directory of the pinned diffusers config repo for
/// the channel mode (9-ch -> inpaint config, 4-ch -> base config).
pub const HEADER_SDXL_CONFIG_DIR: &str = "sdxl_config_dir";
/// `inpaint.sdxl` (v12): absolute directory of the curated fp16 inpaint model; required iff
/// `params.model_path` is the curated repo id, otherwise `null`.
pub const HEADER_SDXL_MODEL_DIR: &str = "sdxl_model_dir";

/// Builds a `hello` frame header (`{ v, id: 0, kind: "hello" }`).
#[must_use]
pub fn hello_header() -> Value {
    json!({
        HEADER_VERSION: PROTOCOL_VERSION,
        HEADER_ID: 0,
        HEADER_KIND: KIND_HELLO,
    })
}

/// Builds a `request` frame header for `method` and correlation `id`, merging in
/// the caller-supplied inline `fields` (method params). `fields` must be a JSON
/// object (or `null`); any non-object value is ignored. The reserved keys
/// (`v`/`id`/`kind`/`method`) always win over `fields`.
#[must_use]
pub fn request_header(id: u64, method: &str, fields: &Value) -> Value {
    let mut map: Map<String, Value> = match fields {
        Value::Object(obj) => obj.clone(),
        _ => Map::new(),
    };
    map.insert(HEADER_VERSION.to_string(), json!(PROTOCOL_VERSION));
    map.insert(HEADER_ID.to_string(), json!(id));
    map.insert(HEADER_KIND.to_string(), json!(KIND_REQUEST));
    map.insert(HEADER_METHOD.to_string(), json!(method));
    Value::Object(map)
}

/// Builds a `cancel` frame header (`{ v, id, kind: "cancel" }`).
#[must_use]
pub fn cancel_header(id: u64) -> Value {
    json!({
        HEADER_VERSION: PROTOCOL_VERSION,
        HEADER_ID: id,
        HEADER_KIND: KIND_CANCEL,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path of the Python mirror of this file, relative to the crate manifest directory.
    /// The mirror lives in the repository root, two levels above `crates/ms-backend-ipc/`.
    const PYTHON_PROTOCOL_REL_PATH: &str = "../../modules/ai_backend/ipc/protocol.py";

    /// Extracts the `PROTOCOL_VERSION = <int>` assignment from Python source.
    ///
    /// Matches only a top-level assignment (no leading indentation), so the surrounding
    /// banner comments that also mention the name cannot be picked up. Returns `None`
    /// when no such assignment exists or its value is not a plain decimal integer.
    fn parse_python_protocol_version(source: &str) -> Option<u32> {
        source
            .lines()
            .filter_map(|line| line.strip_prefix("PROTOCOL_VERSION"))
            .filter_map(|rest| rest.trim_start().strip_prefix('='))
            .map(|rest| {
                // Trim the value and drop a trailing `# …` comment before parsing.
                let value = rest.trim();
                value.split('#').next().unwrap_or(value).trim()
            })
            .find_map(|value| value.parse::<u32>().ok())
    }

    /// `PROTOCOL_VERSION` is the SINGLE point of coupling between the Rust application and
    /// the Python backend: it is the only value the `hello` handshake compares, and a silent
    /// divergence between the two mirrored constants would make every backend call fail at
    /// connect. Nothing else in the build checks that the mirror holds, so this test does.
    #[test]
    fn python_protocol_version_matches_rust() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(PYTHON_PROTOCOL_REL_PATH);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(err) => panic!(
                "cannot read the Python protocol mirror at {}: {err}. \
                 This file defines the Python side of PROTOCOL_VERSION; if it moved, update \
                 PYTHON_PROTOCOL_REL_PATH in crates/ms-backend-ipc/src/protocol.rs together with it.",
                path.display()
            ),
        };
        let Some(python_version) = parse_python_protocol_version(&source) else {
            panic!(
                "no top-level `PROTOCOL_VERSION = <int>` assignment found in {}. \
                 The Rust side declares {PROTOCOL_VERSION}; the mirror cannot be verified.",
                path.display()
            );
        };
        assert_eq!(
            python_version,
            PROTOCOL_VERSION,
            "PROTOCOL_VERSION diverged: crates/ms-backend-ipc/src/protocol.rs declares {PROTOCOL_VERSION}, \
             {} declares {python_version}. These two constants are the only compatibility gate \
             between the application and the Python backend and must be bumped together.",
            path.display()
        );
    }

    #[test]
    fn python_protocol_version_parser_ignores_comments_and_indentation() {
        // The real file surrounds the assignment with banner comments naming the constant,
        // and an indented occurrence would belong to some unrelated scope.
        let source = "# PROTOCOL_VERSION is bumped on breaking changes\n\
                      PROTOCOL_VERSION = 7  # bumped for the blob rework\n\
                      \x20   PROTOCOL_VERSION = 99\n";
        assert_eq!(parse_python_protocol_version(source), Some(7));
        assert_eq!(parse_python_protocol_version("PROTOCOL_VERSION = later\n"), None);
        assert_eq!(parse_python_protocol_version("nothing here\n"), None);
    }

    #[test]
    fn request_header_sets_reserved_fields() {
        let header = request_header(7, METHOD_OCR_MANGA, &json!({ "join_newlines": true }));
        assert_eq!(header[HEADER_VERSION], json!(PROTOCOL_VERSION));
        assert_eq!(header[HEADER_ID], json!(7));
        assert_eq!(header[HEADER_KIND], json!(KIND_REQUEST));
        assert_eq!(header[HEADER_METHOD], json!(METHOD_OCR_MANGA));
        assert_eq!(header["join_newlines"], json!(true));
    }
}
