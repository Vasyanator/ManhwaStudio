"""
File: modules/ai_backend/ipc/protocol.py

Purpose:
Shared constants for the framed, multiplexed Rust <-> Python AI-backend IPC
protocol (v2). This is the single Python-side source of truth for the wire
contract: protocol version, frame kinds, event topics, method names, and the
frame size guards. The Rust side mirrors these exact string values.

Transport:
The protocol runs over a single AF_UNIX domain socket (the base backend socket
path). Each frame is `[u32 BE header_len][header_json][u32 BE blob_len][blob]`;
binary image data travels in `blob` (no base64). The authoritative human-readable
specification lives next to this file in `PROTOCOL.md`.

Notes:
This module is constants ONLY: no logic, no socket code, no frame codec. It is
safe to import from any phase (codec, dispatcher, client) without side effects.
"""

from __future__ import annotations

# ============================================================================
# PROTOCOL VERSION
# ----------------------------------------------------------------------------
# MUST be bumped on ANY change to the contract, not only on one judged breaking:
# a new method, a new header or payload field, a new topic, a changed meaning of
# an existing field, a changed blob format. Deciding whether a change "really"
# breaks anything is exactly the judgement that gets made wrong, and bumping
# costs nothing because both halves ship and update together.
# Bump it here AND in crates/ms-backend-ipc/src/protocol.rs together — the Rust test
# python_protocol_version_matches_rust guards the mirror, but nothing can detect
# a bump that was never made. The hello handshake compares this value; a
# mismatch is a clean error (see PROTOCOL.md "Error model").
# The contract also covers the on-disk user_config storage semantics shared with
# Rust (docstore.py / ms-docstore: `.json` or SQLite `.db`).
# 9: General.android_native_windows replaced by General.secondary_windows
# (user_config migration on load).
# 10: new General.touch_gestures boolean (canvas finger gestures; per-platform
# default).
# 11: Hotkeys section format 2 ({"format": 2, "actions": {id: [alternative, ...]}},
# written by Rust ms-input; a version-1 section is migrated and rewritten on studio open).
# 12: EasyOCR / Surya / SDXL load offline from Rust-downloaded directories: new
# required fields `easyocr_model_dir` (ocr.easy), `surya_recognition_dir` +
# `surya_detection_dir` (ocr.surya), `surya_detection_dir`
# (textdetector.surya.forward), `sdxl_config_dir` (inpaint.sdxl), plus
# `sdxl_model_dir` (inpaint.sdxl, required for the curated repo id);
# `easy_langs` is no longer aliased; other SDXL repo ids are refused.
# 13: large-model pool: new methods `large_models.profiles` / `.limits.set` /
# `.status` / `.unload` / `.learned.reset`; the `health` payload's
# `model_manager` object gains `small_models`, `large_models`, `limits`, `budget`;
# new `user_config` keys `General.large_models_{vram_cap_bytes, ram_cap_bytes}`
# (written by the backend) and `General.large_models_oom_score_adj` (read at
# backend start). Residency is chosen on two step axes, `vram_step` x `ram_step`
# (`.profiles` request and answer, `.status` entries); `inpaint.flux2_klein`
# gains the optional step fields additively.
# 14: the FLUX.2 klein placement wire is retired: `inpaint.flux2_klein*` params
# refuse `placement`, `profile`, `low_cpu_mem_usage`, `vae_slicing`,
# `unload_transformer_before_vae`, `unload_text_encoder_after_encode` by name;
# `applied` is `{vae_tiling}`; the run answer loses `placement`, `profile`,
# `profile_effective`; `.status` loses `memory`; `inpaint.flux2_klein.estimate`
# is removed (forecasts are `large_models.profiles`); the text encoder is never
# pinned to the CPU and the `….cpu` encoder registrations are gone; new
# `user_config` default `General.large_models_oom_score_adj: true`.
# 15: FLUX.3 klein region editing: new methods `inpaint.flux3_klein` (streaming,
# blob region ++ mask ++ references, frames with a stable `stage`) / `.status` /
# `.unload` / `.download.check` / `.download.start`; `large_models.profiles`
# serves `model: "flux3_klein"`; the `health` payload gains `inpaint.flux3_klein`;
# «Энкодер на CPU»: optional `encoder_on_cpu` (bool, absent = true) in the
# `inpaint.flux2_klein*` params, the FLUX.3 params and their `large_models.profiles`
# params (the text encoder then runs on the CPU at every VRAM step).
# 16: local color match of the region editors: optional `color_match_local_pct`
# (0..100, default 60) and `color_match_scale_px` (8..96, default 24, Gaussian
# sigma of the finest level) in the
# `inpaint.flux2_klein` and `inpaint.flux3_klein` params (effective only with
# `color_match`, off under `whole_region`); the global `color_match` now samples
# outside the mask grown by 6 px.
# 17: the FLUX.2 klein and FLUX.3 klein region post-processing is the client's:
# both answers are the RAW generated region (RGB PNG; no colour match, no
# composite, so outside the mask the FLUX.2 answer is a VAE round trip); the
# `inpaint.flux2_klein` mask is the latent mask, already dilated by the client,
# and its params refuse `mask_dilate_px`, `mask_feather_px`, `color_match`,
# `color_match_local_pct`, `color_match_scale_px` by name; those four post
# fields are unknown keys of the `inpaint.flux3_klein` params (refused), whose
# mask is validated only; the FLUX.3 decode rounds to uint8 instead of truncating.
# 18: the `health` payload gains `torch` (null, or {flavour, version,
# hip_version, cuda_version, devices: [{index, name, gcn_arch, integrated}],
# rocm_device_archs, rocm_split_kernels}: the backend's PyTorch build and
# visible GPUs; absent from the warming-up form); new user_config keys
# `General.torch_flavour` /
# `General.torch_rocm_gfx` (the installer's recorded torch choice, written by
# the installer, never read by the backend).
# 19: user_config `Canvas.{aside_compact_mode, aside_side_mode,
# aside_second_column, scale_bubbles, aside_min_width_px, aside_max_width_px,
# spellcheck_original, spellcheck_translation}` are no longer written (the
# canvas layout is per title; the six layout keys are read only as the
# migration source of titles without `canvas.layout_per_title`; the two
# spellcheck copies were never read); new keys `Canvas.layout_presets` (the
# app-wide layout preset store, written by the Settings pane) and
# `TranslarionTab.composition_presets` (the composition panel's app-wide
# presets, written by the Translation tab). The backend reads none of them.
# 20: the `model_load` event topic is removed (it was declared but never
# published or subscribed to); the valid topics are `health`, `device`, `log`.
# 21: FLUX.2 klein weight formats: the `inpaint.flux2_klein*` params and their
# `large_models.profiles` params gain optional `transformer_format` /
# `transformer_formats` and `text_encoder_format` / `text_encoder_formats`
# (selected id + candidates [{id, path}], best first, ids ^[a-z0-9_]{1,32}$
# opaque to the backend; a present list must hold the selected id with `path`
# equal to that component's `*_path`; absent, the id is derived from the
# container: `bf16` for safetensors or a folder, `gguf` for a GGUF file);
# `transformer_path` and `text_encoder_path` may name a `.gguf` file
# (`text_encoder_fp8: true` with a GGUF encoder is refused);
# `inpaint.flux2_klein.download.*` refuses `variant: "4b"` (the app's model
# catalog downloads 4B); the FLUX.2 `large_models.profiles` answer gains
# additive `encoder_formats`, and its `encoder: null` now means "not needed AND
# not resident" (a resident encoder is counted even when the prompt is cached;
# `selected` stays the pipeline's plan, the client sums the two; each of the
# two `forecast` objects gains additive `host_baseline`, the baseline RSS its
# `host_peak` includes, so the sum counts the process once); FLUX.2
# `phase:"load"` frames carry the additive byte level `file_step` /
# `file_total` / `file_label` for the text encoder too (on step 7,
# `LOAD_STEP_TEXT_ENCODER`) and for a transformer folder (on step 1;
# `file_label` = the file or folder name; a read inside a later phase rides on
# the step current then, a `generate` frame during the denoise), plus additive
# `file_component` (`"transformer"` / `"text_encoder"`; absent = transformer).
# FLUX.3 klein: every checkpoint read of `inpaint.flux3_klein` adds the same
# byte level to the `phase:"load"` frame of the current stage (`stage:"denoise"`
# for a reload during the denoise): `file_step` / `file_total` (bytes of this
# read), `file_label` (file name; folder name when sharded) and
# `file_component` (`transformer` = DiT, `text_encoder`, `vae` = AE).
# Load byte frames of both klein models: at most one per 0.2 s per read, plus
# one 100% frame.
# 22: `inpaint.flux2_klein.download.check` / `.start` are removed (the app's
# model catalog downloads every FLUX.2 klein weight, 4B and 9B; the old names
# are unknown methods). No other wire change.
# ============================================================================
PROTOCOL_VERSION = 22

# ============================================================================
# FRAME SIZE GUARDS
# ----------------------------------------------------------------------------
# Hard upper bounds enforced by the codec before allocating/reading a frame.
# A frame declaring a larger header or blob is rejected as a protocol error.
# ============================================================================
MAX_HEADER_BYTES = 1 * 1024 * 1024   # 1 MiB cap on the header_json segment.
MAX_BLOB_BYTES = 32 * 1024 * 1024    # 32 MiB cap on the binary blob segment.

# ============================================================================
# FRAME KINDS
# ----------------------------------------------------------------------------
# The `kind` header field. See PROTOCOL.md for the lifecycle of each kind.
# ============================================================================
KIND_HELLO = "hello"        # Handshake (both directions), id=0.
KIND_REQUEST = "request"    # Client -> server call; carries `method`.
KIND_RESPONSE = "response"  # Server -> client terminal reply; carries `status`.
KIND_PROGRESS = "progress"  # Server -> client interim update for a request id.
KIND_EVENT = "event"        # Server -> client unsolicited push, id=0; `topic`.
KIND_CANCEL = "cancel"      # Client -> server cancellation of a request id.
KIND_ERROR = "error"        # Protocol-level error (framing/version/parse).

VALID_KINDS = frozenset(
    {
        KIND_HELLO,
        KIND_REQUEST,
        KIND_RESPONSE,
        KIND_PROGRESS,
        KIND_EVENT,
        KIND_CANCEL,
        KIND_ERROR,
    }
)

# ============================================================================
# RESPONSE STATUS VALUES
# ----------------------------------------------------------------------------
# The `status` header field on a `response` frame.
# ============================================================================
STATUS_OK = "ok"                    # Request completed; result fields inline.
STATUS_ERROR = "error"              # Request failed; `error` string set.
STATUS_INTERRUPTED = "interrupted"  # Request canceled (client `cancel`).

VALID_STATUSES = frozenset({STATUS_OK, STATUS_ERROR, STATUS_INTERRUPTED})

# ============================================================================
# EVENT TOPICS
# ----------------------------------------------------------------------------
# The `topic` header field on an `event` frame (id=0). See PROTOCOL.md
# "Event topics" for each payload shape.
# ============================================================================
TOPIC_HEALTH = "health"          # Periodic health snapshot push.
TOPIC_DEVICE = "device"          # Device/provider selection state changed.
TOPIC_LOG = "log"                # Optional backend log line stream.

VALID_TOPICS = frozenset(
    {
        TOPIC_HEALTH,
        TOPIC_DEVICE,
        TOPIC_LOG,
    }
)

# ============================================================================
# METHOD NAMES
# ----------------------------------------------------------------------------
# One constant per current backend endpoint. Dotted `namespace.action` form.
# The mapping from legacy HTTP path -> method is documented exhaustively in
# PROTOCOL.md "Method table".
# ============================================================================

# --- OCR ---
METHOD_OCR_MANGA = "ocr.manga"              # POST /ocr/manga
METHOD_OCR_EASY = "ocr.easy"                # POST /ocr/easy
METHOD_OCR_PADDLE = "ocr.paddle"            # POST /ocr/paddle
METHOD_OCR_PADDLE_VL = "ocr.paddle_vl"      # POST /ocr/paddle_vl
METHOD_OCR_SURYA = "ocr.surya"              # POST /ocr/surya
METHOD_OCR_PADDLE_ONNX = "ocr.paddle_onnx"  # POST /ocr/paddle_onnx
# Baberu OCR on ONNX Runtime (v7; vision on the selected ONNX provider, decoders on
# CPU): the backend fallback of the native Rust engine; model files arrive as
# absolute paths in `baberu_model_files`.
METHOD_OCR_BABERU = "ocr.baberu"

# --- Machine translation ---
METHOD_TRANSLATE_DEEP = "translate.deep"    # POST /translate/deep

# --- Inpaint ---
METHOD_INPAINT_LAMA_V2 = "inpaint.lama_v2"                  # POST /inpaint/lama_v2
METHOD_INPAINT_LAMA_V2_UNLOAD = "inpaint.lama_v2.unload"    # POST /inpaint/lama_v2/unload
METHOD_INPAINT_LAMA_MPE = "inpaint.lama_mpe"               # POST /inpaint/lama_mpe
METHOD_INPAINT_LAMA_MPE_UNLOAD = "inpaint.lama_mpe.unload"  # POST /inpaint/lama_mpe/unload
METHOD_INPAINT_AOT = "inpaint.aot"                          # POST /inpaint/aot
METHOD_INPAINT_AOT_UNLOAD = "inpaint.aot.unload"            # POST /inpaint/aot/unload
METHOD_INPAINT_SDXL = "inpaint.sdxl"                        # POST /inpaint/sdxl (streams progress)
METHOD_INPAINT_SDXL_UNLOAD = "inpaint.sdxl.unload"          # POST /inpaint/sdxl/unload
METHOD_INPAINT_FLUX_FILL = "inpaint.flux_fill"             # FLUX.1-Fill (streams download+gen)
METHOD_INPAINT_FLUX_FILL_UNLOAD = "inpaint.flux_fill.unload"
METHOD_INPAINT_FLUX_FILL_STATUS = "inpaint.flux_fill.status"  # quant catalog + download state
# FLUX.2 klein 9B region editing: the mask is a PERMISSION TO CHANGE and every
# pixel outside it comes back byte-identical. All weights are user-supplied
# paths, so there is no download phase — the streamed phases are `load`/`generate`.
METHOD_INPAINT_FLUX2_KLEIN = "inpaint.flux2_klein"                    # streams load+gen progress
METHOD_INPAINT_FLUX2_KLEIN_STATUS = "inpaint.flux2_klein.status"      # components + residency
METHOD_INPAINT_FLUX2_KLEIN_UNLOAD = "inpaint.flux2_klein.unload"
# Per-component residency actions. `.status` reports, for the text encoder, the
# transformer and the VAE, where the weights are and which actions are possible;
# this method performs ONE of those actions. Streaming, because loading the
# encoder takes tens of seconds, and it claims the same single progress bar a generation
# does, so the two can never run at once.
METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION = "inpaint.flux2_klein.component_action"
# Prompt-cache library: a prompt embedding costs a 16 GB encoder read to produce
# and ~4 MiB to keep, so it can be built without generating, stored in
# `<program root>/prompt_cache/<encoder family>/<name>.msprompt`, listed, loaded
# back, and moved in and out of that library.
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD = "inpaint.flux2_klein.prompt_cache.build"
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST = "inpaint.flux2_klein.prompt_cache.list"
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE = "inpaint.flux2_klein.prompt_cache.save"
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD = "inpaint.flux2_klein.prompt_cache.load"
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT = "inpaint.flux2_klein.prompt_cache.export"
METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT = "inpaint.flux2_klein.prompt_cache.import"
# FLUX.3 Image Klein region editing (`ipc/handlers/flux3_klein.py`, PROTOCOL.md
# "FLUX.3 klein"): the region is reference #0, the composite stays inside the
# mask. The run streams `load` / `generate` frames with a stable `stage` and
# cancels mid-run; the package comes from the gated repository per format.
METHOD_INPAINT_FLUX3_KLEIN = "inpaint.flux3_klein"                    # streams load+gen progress
METHOD_INPAINT_FLUX3_KLEIN_STATUS = "inpaint.flux3_klein.status"      # package files + health
METHOD_INPAINT_FLUX3_KLEIN_UNLOAD = "inpaint.flux3_klein.unload"
METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK = "inpaint.flux3_klein.download.check"
METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START = "inpaint.flux3_klein.download.start"

# --- Visible watermark removal ---
# Primary flow is `watermark.detect` (mask only, fed to the existing inpainters);
# `watermark.remove` is the experimental direct network pass. Both stream the
# FLUX-style two-phase progress (`download` bytes / `generate` tiles).
METHOD_WATERMARK_DETECT = "watermark.detect"  # predicted watermark mask PNG
METHOD_WATERMARK_REMOVE = "watermark.remove"  # cleaned image ++ mask (experimental)
METHOD_WATERMARK_STATUS = "watermark.status"  # model catalog + on-disk state
METHOD_WATERMARK_UNLOAD = "watermark.unload"  # drop the resident network

# --- Text detection (forward-only since v4) ---
# Rust sends N equal-size RGB u8 tiles it already resized/padded and receives
# u8 probability maps; planning and post-processing live in Rust. The v3
# page-level methods `textdetector.ctd` / `.paddle` / `.surya` were removed.
METHOD_TEXTDETECTOR_CTD_FORWARD = "textdetector.ctd.forward"        # maps [seg, shrink]
METHOD_TEXTDETECTOR_PADDLE_FORWARD = "textdetector.paddle.forward"  # maps [prob]
METHOD_TEXTDETECTOR_SURYA_FORWARD = "textdetector.surya.forward"    # maps [text] at 1/4

# --- Device ---
METHOD_DEVICE_GET = "device.get"                          # GET /device
METHOD_DEVICE_SET = "device.set"                          # POST /device/set
METHOD_DEVICE_CUDA_DIAGNOSTICS = "device.cuda_diagnostics"  # GET /device/cuda_diagnostics

# --- Reline ---
METHOD_RELINE_MODELS = "reline.models"    # GET /reline/models
METHOD_RELINE_PROCESS = "reline.process"  # POST /reline/process

# --- Browser scraping (Selenium / CloakBrowser) ---
# Single multiplexed method that carries the legacy advanced-download command
# object (`{"command": "open_url"|"fetch"|..., ...}`) inside the request header
# `payload` field. The unified backend hosts the browser session in-process (see
# `modules/ai_backend/browser/service.py`); progress streams as `progress` frames
# and the daemon's terminal event dict is returned as the response header.
METHOD_BROWSER_COMMAND = "browser.command"  # was: adv_fetch_cli.py / adv_fetch_cloak_cli.py stdio

# --- Large-model pool (v13) ---
# Model-agnostic planning and residency of the large models (FLUX.2 / FLUX.3
# klein) through `AppState.large_models` (`runtime/large_models/manager.py`).
# `.profiles` is read-only (probes and plans, never loads); the others change the
# pool or the user's limits. Request/response shapes: PROTOCOL.md §5.10.
METHOD_LARGE_MODELS_PROFILES = "large_models.profiles"  # VRAM / RAM step pickers (fit + forecast) for one request
METHOD_LARGE_MODELS_LIMITS_SET = "large_models.limits.set"  # hard VRAM / RAM caps (persisted)
METHOD_LARGE_MODELS_STATUS = "large_models.status"  # loaded handles and the limits in force (floors: .profiles)
METHOD_LARGE_MODELS_UNLOAD = "large_models.unload"  # drop idle handles (all, or one model's)
METHOD_LARGE_MODELS_LEARNED_RESET = "large_models.learned.reset"  # forget learned step floors

# --- Health ---
# Health is primarily pushed via TOPIC_HEALTH events, but a request/response
# form is kept so a freshly connected client can pull the current snapshot.
METHOD_HEALTH = "health"  # GET /health

ALL_METHODS = frozenset(
    {
        METHOD_OCR_MANGA,
        METHOD_OCR_EASY,
        METHOD_OCR_PADDLE,
        METHOD_OCR_PADDLE_VL,
        METHOD_OCR_SURYA,
        METHOD_OCR_PADDLE_ONNX,
        METHOD_OCR_BABERU,
        METHOD_TRANSLATE_DEEP,
        METHOD_INPAINT_LAMA_V2,
        METHOD_INPAINT_LAMA_V2_UNLOAD,
        METHOD_INPAINT_LAMA_MPE,
        METHOD_INPAINT_LAMA_MPE_UNLOAD,
        METHOD_INPAINT_AOT,
        METHOD_INPAINT_AOT_UNLOAD,
        METHOD_INPAINT_SDXL,
        METHOD_INPAINT_SDXL_UNLOAD,
        METHOD_INPAINT_FLUX_FILL,
        METHOD_INPAINT_FLUX_FILL_UNLOAD,
        METHOD_INPAINT_FLUX_FILL_STATUS,
        METHOD_INPAINT_FLUX2_KLEIN,
        METHOD_INPAINT_FLUX2_KLEIN_STATUS,
        METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
        METHOD_INPAINT_FLUX2_KLEIN_COMPONENT_ACTION,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
        METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
        METHOD_INPAINT_FLUX3_KLEIN,
        METHOD_INPAINT_FLUX3_KLEIN_STATUS,
        METHOD_INPAINT_FLUX3_KLEIN_UNLOAD,
        METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK,
        METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START,
        METHOD_WATERMARK_DETECT,
        METHOD_WATERMARK_REMOVE,
        METHOD_WATERMARK_STATUS,
        METHOD_WATERMARK_UNLOAD,
        METHOD_TEXTDETECTOR_CTD_FORWARD,
        METHOD_TEXTDETECTOR_PADDLE_FORWARD,
        METHOD_TEXTDETECTOR_SURYA_FORWARD,
        METHOD_DEVICE_GET,
        METHOD_DEVICE_SET,
        METHOD_DEVICE_CUDA_DIAGNOSTICS,
        METHOD_RELINE_MODELS,
        METHOD_RELINE_PROCESS,
        METHOD_BROWSER_COMMAND,
        METHOD_LARGE_MODELS_PROFILES,
        METHOD_LARGE_MODELS_LIMITS_SET,
        METHOD_LARGE_MODELS_STATUS,
        METHOD_LARGE_MODELS_UNLOAD,
        METHOD_LARGE_MODELS_LEARNED_RESET,
        METHOD_HEALTH,
    }
)

# ============================================================================
# HEADER FIELD NAMES
# ----------------------------------------------------------------------------
# Canonical key names inside `header_json`, so both sides avoid typos.
# ============================================================================
HEADER_VERSION = "v"        # int: protocol version.
HEADER_ID = "id"            # u64: correlation id (0 == server-initiated event).
HEADER_KIND = "kind"        # str: one of VALID_KINDS.
HEADER_METHOD = "method"    # str: request method (ALL_METHODS).
HEADER_TOPIC = "topic"      # str: event topic (VALID_TOPICS).
HEADER_STATUS = "status"    # str: response status (VALID_STATUSES).
HEADER_ERROR = "error"      # str: error message.
HEADER_BACKEND_VERSION = "backend_version"  # str: hello reply backend version.
