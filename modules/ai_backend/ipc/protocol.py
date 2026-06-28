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
# Bumped on any breaking change to the frame layout, header fields, kinds,
# topics, or method contracts. The hello handshake compares this value; a
# mismatch is a clean error (see PROTOCOL.md "Error model").
# ============================================================================
PROTOCOL_VERSION = 1

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
TOPIC_MODEL_LOAD = "model_load"  # Model load/unload progress.
TOPIC_LOG = "log"                # Optional backend log line stream.

VALID_TOPICS = frozenset(
    {
        TOPIC_HEALTH,
        TOPIC_DEVICE,
        TOPIC_MODEL_LOAD,
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

# --- Text detection ---
METHOD_TEXTDETECTOR_CTD = "textdetector.ctd"        # POST /textdetector/ctd/detect
METHOD_TEXTDETECTOR_PADDLE = "textdetector.paddle"  # POST /textdetector/paddle/detect
METHOD_TEXTDETECTOR_SURYA = "textdetector.surya"    # POST /textdetector/surya/detect

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
        METHOD_TEXTDETECTOR_CTD,
        METHOD_TEXTDETECTOR_PADDLE,
        METHOD_TEXTDETECTOR_SURYA,
        METHOD_DEVICE_GET,
        METHOD_DEVICE_SET,
        METHOD_DEVICE_CUDA_DIAGNOSTICS,
        METHOD_RELINE_MODELS,
        METHOD_RELINE_PROCESS,
        METHOD_BROWSER_COMMAND,
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
