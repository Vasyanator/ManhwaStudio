/*
File: backend_ipc/protocol.rs

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
/// `protocol.PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: u32 = 1;

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

// --- Text detection ---
pub const METHOD_TEXTDETECTOR_CTD: &str = "textdetector.ctd";
pub const METHOD_TEXTDETECTOR_PADDLE: &str = "textdetector.paddle";
pub const METHOD_TEXTDETECTOR_SURYA: &str = "textdetector.surya";

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
