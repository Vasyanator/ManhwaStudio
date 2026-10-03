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
/// carries a different value. The program version (`CARGO_PKG_VERSION` /
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
pub const PROTOCOL_VERSION: u32 = 4;

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
pub const TOPIC_MODEL_LOAD: &str = "model_load";
pub const TOPIC_LOG: &str = "log";

// ============================================================================
// METHOD NAMES (dotted namespace.action form)
// ============================================================================

// --- OCR ---
pub const METHOD_OCR_MANGA: &str = "ocr.manga";
pub const METHOD_OCR_EASY: &str = "ocr.easy";
pub const METHOD_OCR_PADDLE: &str = "ocr.paddle";
pub const METHOD_OCR_PADDLE_VL: &str = "ocr.paddle_vl";
pub const METHOD_OCR_SURYA: &str = "ocr.surya";
pub const METHOD_OCR_PADDLE_ONNX: &str = "ocr.paddle_onnx";

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
/// `mask_len` and `params`; the blob is `region.png ++ mask.png` (mask L8, exactly
/// the region size). The response header carries `image_len` and the blob is the
/// edited region as an RGB PNG of exactly the region size.
pub const METHOD_INPAINT_FLUX2_KLEIN: &str = "inpaint.flux2_klein";
/// Reports whether the FLUX.2 klein components resolve on disk plus the host's
/// current VRAM/RAM figures.
pub const METHOD_INPAINT_FLUX2_KLEIN_STATUS: &str = "inpaint.flux2_klein.status";
/// Predicts the VRAM/RAM cost of one FLUX.2 klein run for the given `params` and
/// region size, and whether it fits.
pub const METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE: &str = "inpaint.flux2_klein.estimate";
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
