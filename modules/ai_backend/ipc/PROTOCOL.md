# ManhwaStudio Backend IPC Protocol v2

Authoritative specification for the framed, multiplexed, bidirectional message
protocol between the Rust frontend (client) and the Python AI backend (server)
over a pluggable transport (AF_UNIX by default, loopback WebSocket fallback on
Windows).

This document is the single source of truth. Both sides are implemented purely
from it. The Python constants live in `protocol.py`; the Rust side mirrors the
same string/number values. Any field listed here is part of the contract.

- **Protocol version:** `5` (`PROTOCOL_VERSION`). This is the ONLY compatibility
  gate between the two halves: it is compared in the `hello` handshake and lives in
  `protocol.py` mirrored by `crates/ms-backend-ipc/src/protocol.rs`. It MUST be bumped in BOTH
  files on ANY change to this contract, not only on one judged breaking — a new method,
  a new header or payload field, a new topic, a changed meaning of an existing field, a
  changed blob format. A Rust parity test asserts the two constants agree, but nothing
  can detect a bump that was never made. The program version is never compared (see
  `backend_version` in §2 and §3.1).
- **Transport:** pluggable and platform-selected — AF_UNIX by default (unix),
  loopback WebSocket fallback on Windows. A single persistent connection,
  multiplexed by correlation id. The frame codec is transport-agnostic
  (identical bytes on either carrier; see the WebSocket carrier note in §1).
- **Encoding:** UTF-8 JSON header + raw binary blob (no base64 on the wire).

---

## 1. Frame wire format

Every message — in either direction — is a single frame:

```
+------------------+----------------------------+----------------+------------------+
| u32 BE           | header_json                | u32 BE         | blob             |
| header_len       | (header_len bytes, UTF-8)  | blob_len       | (blob_len bytes) |
+------------------+----------------------------+----------------+------------------+
```

Byte layout, in order:

1. `header_len`: unsigned 32-bit, **big-endian**. Length in bytes of the
   `header_json` segment.
2. `header_json`: exactly `header_len` bytes, a UTF-8 encoded JSON **object**.
3. `blob_len`: unsigned 32-bit, **big-endian**. Length in bytes of the `blob`
   segment. **May be 0** (no blob).
4. `blob`: exactly `blob_len` bytes of raw binary data (e.g. PNG bytes). Absent
   when `blob_len == 0`.

### Size guards

Enforced before allocating or reading either segment. A frame that declares a
larger segment is a fatal protocol error (the reader closes the connection or
emits a `kind:"error"` frame; see §6):

| Guard               | Value          | Constant            |
|---------------------|----------------|---------------------|
| Max header bytes    | 1 MiB          | `MAX_HEADER_BYTES`  |
| Max blob bytes      | 32 MiB         | `MAX_BLOB_BYTES`    |

There is at most **one** blob per frame. Requests/responses that need to move
binary data put it in this blob; everything else (sizes, params, results,
strings, base64-free metadata) lives in `header_json`.

### WebSocket carrier

Over the loopback-WebSocket transport the identical frame bytes ride inside WS
BINARY messages. The receiver treats **all** binary payloads as one ordered byte
stream and delimits frames by the length prefixes above; it does **not** assume
one WS message == one frame (a frame may span several WS messages, or one WS
message may carry parts of several frames). The frame layout is unchanged — the
WebSocket is only a carrier.

---

## 2. Header field reference

`header_json` is always a JSON object. Fields by usage:

| Field             | Type   | Required on            | Meaning                                                                 |
|-------------------|--------|------------------------|-------------------------------------------------------------------------|
| `v`               | int    | `hello`                | Protocol version. `PROTOCOL_VERSION` = 5. Optional/ignored on others.   |
| `id`              | u64    | all framed messages    | Correlation id. `0` means a server-initiated frame (events, hello).     |
| `kind`            | string | all                    | One of `hello`,`request`,`response`,`progress`,`event`,`cancel`,`error`.|
| `method`          | string | `request`              | Method name, e.g. `ocr.manga`. See §5.                                  |
| `topic`           | string | `event`                | Event topic, e.g. `health`. See §7.                                     |
| `status`          | string | `response`             | `ok` \| `error` \| `interrupted`.                                       |
| `error`           | string | `response(error)`,`error` | Human-readable error message.                                        |
| `backend_version` | string | server `hello`         | Backend app version (e.g. `3.4.2`). DIAGNOSTIC ONLY — never compared.    |
| _method params_   | mixed  | `request`              | Inline request fields (see each method in §5).                          |
| _result fields_   | mixed  | `response(ok)`         | Inline result fields (see each method in §5).                           |

**id rules:**
- The **client** picks `id`: a strictly monotonically increasing `u64`, starting
  at 1. Each in-flight request owns a unique id until its terminal `response`.
- Server `progress`, `response`, and `error` frames that answer a request echo
  that request's `id`.
- `id = 0` is reserved for server-initiated `event` frames and for the `hello`
  handshake.

---

## 3. Kinds and lifecycle

### 3.1 `hello` — handshake (id = 0)

On connect, before any request, the **client** sends:

```json
{ "v": 1, "id": 0, "kind": "hello" }
```
(blob_len = 0)

The **server** replies:

```json
{ "v": 1, "id": 0, "kind": "hello", "backend_version": "3.4.2" }
```
(blob_len = 0)

If `v` does not equal the server's `PROTOCOL_VERSION`, the server replies with a
protocol `error` frame (§6) and closes the connection. The client must treat a
version mismatch as a clean, fatal handshake failure (no requests are sent). The
client applies the same rule to the server's `v` in the reply above.

`backend_version` carries the backend's PROGRAM version. It is diagnostic only: the
client logs it and never compares it with its own build version. Two halves whose
`PROTOCOL_VERSION` agrees are compatible regardless of their program versions.

### 3.2 `request` → `progress`* → `response` (correlated by `id`)

The client sends a `request`:

```json
{ "v": 1, "id": 42, "kind": "request", "method": "ocr.manga",
  "join_newlines": true, "reflect_strings": false, "manga_model": "base" }
```
(blob = input image PNG bytes, when the method takes an image)

The server MAY emit zero or more `progress` frames with the same `id` before the
terminal frame:

```json
{ "v": 1, "id": 42, "kind": "progress", "step": 7, "total": 30 }
```
(blob = optional preview PNG bytes for SDXL; otherwise blob_len = 0)

The server then emits exactly one terminal `response`:

```json
{ "v": 1, "id": 42, "kind": "response", "status": "ok",
  "engine": "mangaocr", "lines": ["..."], "text": "..." }
```
(blob = result PNG bytes for inpaint methods, raw u8 maps for the text-detector
forward methods; else blob_len = 0)

After the `response`, the `id` is retired and may not be reused.

### 3.3 `cancel` — real cancellation (client → server)

```json
{ "v": 1, "id": 42, "kind": "cancel" }
```
(blob_len = 0)

`cancel{id}` requests cancellation of the in-flight request with that `id`. This
**replaces** the legacy server-side "latest wins" OCR behavior: the client now
issues an explicit cancel when it abandons a request. The server:
- attempts to stop the work,
- emits a terminal `response` with `status:"interrupted"` for that `id` (it is
  still a valid terminal frame, so the `id` is then retired),
- a cancel for an unknown/already-finished `id` is a no-op.

A new request does **not** implicitly cancel an older one; the client multiplexes
or cancels explicitly.

### 3.4 `event` — unsolicited server push (id = 0)

```json
{ "v": 1, "id": 0, "kind": "event", "topic": "health", "...": "..." }
```

Events are not correlated to any request (`id = 0`). They replace health polling
and add model-load progress and optional log streaming. See §7 for per-topic
payloads. Clients that do not care about a topic ignore it.

### 3.5 `error` — protocol-level error

See §6. Used for framing/parse/version errors that are not tied to a specific
request result (a failed request uses `response{status:"error"}` instead).

---

## 4. Blob convention

- The blob carries **raw bytes**, never base64. The legacy fields
  `image_base64` / `image_b64` / `mask_base64` / `mask_b64` / `image_png_base64`
  / `preview_png_base64` are **removed** from headers; that binary moves to the
  frame blob.
- A method that takes a single image puts it in the **request** blob.
- A method that returns a single image (inpaint result) puts it in the
  **response** blob, and the corresponding `*_png_base64` result field is
  dropped.
- The text-detector forward methods (§5.3) are the one place where the blob is
  NOT encoded at all: N equal-size raw RGB u8 tiles in, raw u8 probability maps
  out, with the dimensions in the header.
- Methods needing **two** input images (inpaint: image + mask) cannot use one
  blob for both. Convention: the **image** goes in the request blob; the **mask**
  PNG bytes go in a second frame field... — see the per-method note in §5: such
  methods send `image` in the blob and keep `mask` as a sibling. Implementers:
  the agreed encoding for the two-image inpaint methods is documented inline in
  §5.4; do not invent a different one.
- The same appendix convention applies in the RESPONSE direction for the one
  method that returns two images, `watermark.remove`: response blob =
  `clean_png ++ mask_png`, split by `image_len` + `mask_len` response header
  ints (§5.9). Same rule, opposite direction — still no base64.

---

## 5. Method table

Legend:
- **blob(req)** = what binary moves into the request frame blob.
- **blob(resp)** = what binary moves into the response frame blob.
- **stream** = whether the server may emit `progress` frames.
- **cancel** = whether `cancel{id}` is honored (true for the latest-wins OCR
  methods and long inpaint/SDXL runs).
- All request header fields are inline in `header_json`.
- The `HTTP path` column is a historical cross-reference to the endpoint each
  method replaced; it is not an address the backend serves. Methods added after
  the HTTP backend was retired have no such column entry.

Total methods mapped: **36** — the full contents of `ALL_METHODS` in
`protocol.py`, which is the machine-readable source of the same list. A method
present in `protocol.py` but absent here is a documentation defect.

### 5.1 OCR

All OCR methods share the image-in-blob input and `lines`/`text` result. They
run on per-engine single-worker queues and honor `cancel` (legacy "latest wins"
becomes explicit cancel).

| HTTP path        | method            | request fields (inline)                                                                                                                                                                                 | blob(req)       | response fields (status=ok)                                 | blob(resp) | stream | cancel |
|------------------|-------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------|-------------------------------------------------------------|------------|--------|--------|
| POST /ocr/manga  | `ocr.manga`       | `join_newlines: bool=true`, `reflect_strings: bool=false`, `manga_model: string\|null`                                                                                                                   | input image PNG | `engine:"mangaocr"`, `lines: string[]`, `text: string`      | none       | no     | yes    |
| POST /ocr/easy   | `ocr.easy`        | `join_newlines: bool=true`, `reflect_strings: bool=false`, `easy_langs: string="ko"`                                                                                                                     | input image PNG | `engine:"easyocr"`, `lines: string[]`, `text: string`       | none       | no     | yes    |
| POST /ocr/paddle | `ocr.paddle`      | `join_newlines: bool=true`, `reflect_strings: bool=false`, `paddle_lang: string="korean_v5"`                                                                                                             | input image PNG | `engine:"paddleocr"`, `lines: string[]`, `text: string`     | none       | no     | yes    |
| POST /ocr/paddle_vl | `ocr.paddle_vl`| `join_newlines: bool=true`, `reflect_strings: bool=false`, `paddle_vl_script: string\|null` (lowercased)                                                                                                  | input image PNG | `engine:"paddleocrvl"`, `lines: string[]`, `text: string`   | none       | no     | yes    |
| POST /ocr/surya  | `ocr.surya`       | `join_newlines: bool=true`, `reflect_strings: bool=false`, `surya_task_name: string="ocr_without_boxes"`, `surya_recognize_math: bool=false`, `surya_sort_lines: bool=false`, `surya_drop_repeated_text: bool=false`, `surya_max_sliding_window: int>0\|null`, `surya_max_tokens: int>0\|null` | input image PNG | `engine:"suryaocr"`, `task_name: string`, `lines: string[]`, `text: string` | none | no | yes |
| POST /ocr/paddle_onnx | `ocr.paddle_onnx` | `join_newlines: bool=true`, `reflect_strings: bool=false`, `paddle_onnx_model: string="korean_v5"` (lowercased), `paddle_onnx_device: string="cpu"` (lowercased)                                    | input image PNG | `engine:"paddleocr_onnx"`, `model: string`, `device: string`, `lines: string[]`, `text: string` | none | no | yes |

Notes:
- `manga_model` value `base_torch` requires Torch (see §6 Torch gate).
- `easy`, `paddle_vl`, `surya` require Torch. `paddle` and `paddle_onnx` run on
  the ONNX runtime and do not require Torch.
- `paddle_onnx_model` is one of the keys in `PADDLE_ONNX_MODEL_TO_LANG`
  (`korean_v5`, `chinese_v5`, ..., `tamil_v3`).

### 5.2 Machine translation

| HTTP path          | method           | request fields (inline)                                                                                                | blob(req) | response fields (status=ok)                                                                  | blob(resp) | stream | cancel |
|--------------------|------------------|------------------------------------------------------------------------------------------------------------------------|-----------|----------------------------------------------------------------------------------------------|------------|--------|--------|
| POST /translate/deep | `translate.deep` | `service: string="google"`, `source: string="auto"`, `target: string="ru"`, `params: object={}`, `texts: string[]` (non-empty) | none      | `service: string`, `translated: int`, `errors: int`, `results: object[]` (each `{ok: bool, ...}`) | none       | no     | no     |

### 5.3 Text detection (forward-only)

The backend runs only the detector **networks**. Rust (`crates/ms-text-detect`)
decides scale and tiling, resizes and pads the page, cuts N equal-size tiles,
and after the call stitches the maps and runs every post-process step (DB
boxes, mask refinement, CRAFT components, dilation). The v3 page-level methods
`textdetector.ctd` / `.paddle` / `.surya` (page path or PNG in, blocks and a mask
PNG out) were removed in v4.

| method                        | request fields (inline)                     | blob(req)                         | response fields (status=ok)                                                                 | blob(resp)       | stream | cancel |
|-------------------------------|---------------------------------------------|-----------------------------------|---------------------------------------------------------------------------------------------|------------------|--------|--------|
| `textdetector.ctd.forward`    | `n: int`, `width: int`, `height: int`       | `n` RGB u8 tiles                  | `engine:"ctd"`, `n: int`, `map_width: int`, `map_height: int`, `channels: ["seg","shrink"]` | u8 maps          | no     | no     |
| `textdetector.paddle.forward` | `n: int`, `width: int`, `height: int`       | `n` RGB u8 tiles                  | `engine:"paddle"`, `n`, `map_width`, `map_height`, `channels: ["prob"]`                     | u8 maps          | no     | no     |
| `textdetector.surya.forward`  | `n: int`, `width: int`, `height: int`       | `n` RGB u8 tiles                  | `engine:"surya"`, `n`, `map_width`, `map_height`, `channels: ["text"]`                      | u8 maps          | no     | no     |

Engine table (both sides hold it: `FORWARD_SPECS` in `handlers/textdetector.py`,
`ForwardEngine` in `crates/ms-backend-ipc/src/textdetector.rs`):

| engine   | align (tile sides multiple of) | map size                     | channels (blob order) | input normalization (backend)                                  |
|----------|--------------------------------|------------------------------|-----------------------|----------------------------------------------------------------|
| `ctd`    | 64                             | `width x height`             | `seg`, `shrink`       | `/255`, RGB, NCHW float32                                      |
| `paddle` | 32                             | `width x height`             | `prob`                | `/255`, ImageNet mean/std, NCHW float32; MIGraphX -> CPU session |
| `surya`  | 4                              | `width/4 x height/4`         | `text`                | the library processor (`/255`, ImageNet mean/std), no resize; float32 on CUDA |

Blob layouts:
- Request: `n * height * width * 3` bytes — RGB u8, tile-major, then row-major,
  then interleaved channels (`RGBRGB...`). All tiles share one size.
- Response: `n * C * map_height * map_width` bytes — tile-major, then
  channel-major (in `channels` order), then row-major. Each value is
  `round(clip(p, 0, 1) * 255)` of the sigmoid probability `p`.

Validation (both sides; a failure is `response{status:"error"}` with a message):
- `n >= 1`; `width` and `height` are positive multiples of the engine align;
- the request blob length equals `n * height * width * 3` exactly (checked
  arithmetic on the Rust side);
- the request blob and the response it implies both fit `MAX_BLOB_BYTES`, so the
  client sizes its batches against BOTH directions;
- the response header matches the request (`engine`, `n`, map size, channel
  names) and the response blob length is exact;
- a non-finite probability anywhere is an error, never a map of zeros.

Notes:
- CTD and Surya require Torch; Paddle uses the ONNX runtime.
- Not cancellable: one bounded forward pass per request; the client bounds the
  batch instead.

### 5.4 Inpaint

All inpaint engines take **two** input images (image + mask) and return one
result image. Encoding convention (v2):
- **`image` PNG bytes go in the request frame blob.**
- **`mask` PNG bytes go in a second `mask` inline field as raw-binary-in-blob is
  single-slot**, so the mask is carried as a length-prefixed appendix: the
  request blob is `image_png` and the header carries `mask_len: int` naming how
  many trailing bytes of the blob are the mask; i.e. **blob = image_png ++
  mask_png**, and `image_len`/`mask_len` header ints split it. Implementers MUST
  use `image_len` + `mask_len` header fields with a concatenated blob; do not
  base64 either image.
- The result image PNG goes in the **response blob** (legacy
  `image_png_base64` field dropped).

| HTTP path                | method                    | request fields (inline)                                                  | blob(req)            | response fields (status=ok)                                                        | blob(resp) | stream | cancel |
|--------------------------|---------------------------|--------------------------------------------------------------------------|----------------------|------------------------------------------------------------------------------------|------------|--------|--------|
| POST /inpaint/lama_v2    | `inpaint.lama_v2`         | `image_len: int`, `mask_len: int`, `params: object` (`refine, n_iters, max_scales, px_budget, model_name`) | image PNG ++ mask PNG | `engine:"lama_v2"`, `source_size: [w,h]`, `device: string`, `refine: bool`, `model_name: string\|null` | result PNG | no     | yes    |
| POST /inpaint/lama_v2/unload | `inpaint.lama_v2.unload` | (none)                                                               | none                 | `unloaded: bool`                                                                    | none       | no     | no     |
| POST /inpaint/lama_mpe   | `inpaint.lama_mpe`        | `image_len: int`, `mask_len: int`, `params: object` (`inpaint_size`)     | image PNG ++ mask PNG | `engine:"lama_mpe"`, `source_size: [w,h]`, `device: string`, `inpaint_size: int`   | result PNG | no     | yes    |
| POST /inpaint/lama_mpe/unload | `inpaint.lama_mpe.unload` | (none)                                                             | none                 | `unloaded: bool`                                                                    | none       | no     | no     |
| POST /inpaint/aot        | `inpaint.aot`             | `image_len: int`, `mask_len: int`, `params: object` (`inpaint_size`)     | image PNG ++ mask PNG | `engine:"aot"`, `source_size: [w,h]`, `device: string`, `inpaint_size: int`        | result PNG | no     | yes    |
| POST /inpaint/aot/unload | `inpaint.aot.unload`      | (none)                                                                   | none                 | `unloaded: bool`                                                                    | none       | no     | no     |
| POST /inpaint/sdxl       | `inpaint.sdxl`            | `image_len: int`, `mask_len: int`, `params: object` (`mode, model_path, positive_prompt, negative_prompt, steps, cfg_scale, denoise_strength, seed, sampler, mask_blur, mask_dilation, lama_model?`) | image PNG ++ mask PNG | `engine:"sdxl"`, `source_size: [w,h]`, `device: string`, `mode: string` | result PNG | **yes** | yes |
| POST /inpaint/sdxl/unload | `inpaint.sdxl.unload`    | (none)                                                                   | none                 | `unloaded: bool`                                                                    | none       | no     | no     |

**SDXL streaming detail** (replaces the legacy NDJSON-over-HTTP frames): the
server emits `progress` frames during diffusion:

```json
{ "v":1, "id":42, "kind":"progress", "step":7, "total":30 }
```
- `step: int`, `total: int`.
- An optional per-step latent **preview PNG** goes in that progress frame's
  **blob** (legacy `preview_png_base64`). `blob_len = 0` when no preview.

The terminal SDXL frame is a normal `response` (`status:"ok"` with the result
PNG in the response blob, or `status:"error"` with `error`).

**FLUX.1-Fill-dev** (no HTTP predecessor; same blob convention as above):

| method                       | request fields (inline)                                                                 | blob(req)             | response fields (status=ok)                                                          | blob(resp) | stream  | cancel |
|------------------------------|------------------------------------------------------------------------------------------|-----------------------|----------------------------------------------------------------------------------------|------------|---------|--------|
| `inpaint.flux_fill`          | `image_len: int`, `mask_len: int`, `params: object` (`mode, quant, prompt, steps, guidance, seed, max_seq, max_side, dilate, feather, seamless, vae_tiling, cpu_offload, miopen_fast, dtype`) | image PNG ++ mask PNG | `engine:"flux_fill"`, `source_size: [w,h]`, `device: string`, `mode: string`, `quant: string` | result PNG | **yes** | yes    |
| `inpaint.flux_fill.unload`   | (none)                                                                                     | none                  | `unloaded: bool`                                                                       | none       | no      | no     |
| `inpaint.flux_fill.status`   | (none)                                                                                     | none                  | `quants: string[]`, `default_quant: string`, `downloaded_quants: string[]`, `components_ready: bool`, `gguf_repo: string`, `components_repo: string` | none | no | no |

**FLUX streaming detail**: unlike SDXL, FLUX reports TWO phases, distinguished
by a `phase` header field, and never carries a preview blob:

```json
{ "v":1, "id":42, "kind":"progress", "phase":"download", "step":10485760, "total":25165824, "label":"flux1-fill-dev-Q4_K_S.gguf" }
```
- `phase:"download"` — `step`/`total` are BYTES, `label` names the file.
- `phase:"generate"` — `step`/`total` are diffusion steps.

**FLUX.2 klein 9B region editing** (no HTTP predecessor; same blob convention):

Unlike every other engine here the mask is a **permission to change**, not a
hole to fill: every pixel outside it comes back byte-identical, and the request
carries a REGION of the page rather than a whole page. The three component paths
are supplied by the user, so generation itself downloads nothing and its streamed
phases are `load`/`generate`. Acquiring those components is a SEPARATE pair of
methods (`download.check` / `download.start`, below), and only `download.start`
streams `phase:"download"`.

| method                          | request fields (inline)                                                                 | blob(req)              | response fields (status=ok)                                            | blob(resp) | stream  | cancel |
|---------------------------------|------------------------------------------------------------------------------------------|------------------------|--------------------------------------------------------------------------|------------|---------|--------|
| `inpaint.flux2_klein`           | `image_len: int`, `mask_len: int`, `reference_len: int?`, `params: object` (see below)     | region PNG ++ mask PNG [++ reference PNG] | `image_len: int`, `oom_recovered: bool`, `applied: object`                 | result PNG | **yes** | yes    |
| `inpaint.flux2_klein.status`    | `params: object={}` (the three paths and `prompt`; may be partial)                         | none                   | `available: bool`, `reason: string\|null`, `components: object`, `components_busy: bool`, `memory: object`, `loaded: bool`, `device: string`, `prompt_cached: bool`, `text_encoder_available: bool`, `guidance_supported: bool` | none | no | no |
| `inpaint.flux2_klein.estimate`  | `params: object`, `region_width: int`, `region_height: int`                                | none                   | `vram_bytes`, `ram_bytes`, `vram_free`, `ram_free`, `fits: bool`, `breakdown: object` | none | no | no |
| `inpaint.flux2_klein.unload`    | (none)                                                                                     | none                   | `unloaded: bool`                                                          | none       | no      | no     |
| `inpaint.flux2_klein.component_action` | `params: object`, `component: string`, `action: string`                             | none                   | `component`, `action`, `performed: bool`, `components: object`, `components_busy: false`, `device: string` | none | **yes** | yes |
| `inpaint.flux2_klein.prompt_cache.build`  | `params: object`                                                         | none                   | `prompt: string`, `encoded: bool`, `prompt_cached: true`, `device: string` | none      | **yes** | yes    |
| `inpaint.flux2_klein.prompt_cache.list`   | `params: object` (`text_encoder_path` optional)                          | none                   | `family: string`, `directory: string`, `entries: array`, `skipped: array`, `text_encoder_available: bool` | none | no | no |
| `inpaint.flux2_klein.prompt_cache.save`   | `params: object`, `name: string`, `overwrite: bool=false`                | none                   | `family`, `name`, `path`, `size_bytes`, `prompt`, `created_at`            | none       | no      | no     |
| `inpaint.flux2_klein.prompt_cache.load`   | `params: object`, `name: string`                                         | none                   | `family`, `name`, `path`, `prompt`, `prompt_cached: true`, `max_sequence_length`, `dtype`, `created_at`, `encoder_verified: bool` | none | no | no |
| `inpaint.flux2_klein.prompt_cache.export` | `params: object`, `name: string`, `path: string` (`*.msprompt`)          | none                   | `family`, `name`, `path`, `size_bytes`                                    | none       | no      | no     |
| `inpaint.flux2_klein.prompt_cache.import` | `params: object`, `path: string` (`*.msprompt`), `name: string?`, `overwrite: bool=false` | none | `family`, `name`, `path`, `size_bytes`, `prompt`, `created_at`, `current_family`, `family_matches: bool` | none | no | no |
| `inpaint.flux2_klein.download.check` | `hf_token: string` (may be empty), `uncensored: bool=false`, `variant: "9b"\|"4b"="9b"`  | none                   | `variant: string`, `repos: object` (repo id -> `{state, message}`), `plan: {total_bytes, missing_bytes, missing_files}\|null`, `plan_error: string` | none | no | no |
| `inpaint.flux2_klein.download.start` | `hf_token: string` (may be empty for `4b`), `uncensored: bool=false`, `variant: "9b"\|"4b"="9b"` | none          | `paths: {transformer, text_encoder, vae}`, `downloaded_bytes: int`, `skipped_files: int` | none | **yes** | yes |

**Model download from Hugging Face** (`dev-docs/flux2_model_download.md` is the
authoritative contract; `inpaint/flux2_download.py` implements it):

**`variant` selects the model, and an absent field means `"9b"`.** That default
is pinned so a client built before the field existed keeps downloading exactly
what it used to. The two values differ in every way that matters:

| `variant` | repository | gated | plan | uncensored encoder | destination |
|---|---|---|---|---|---|
| `"9b"` (default) | `black-forest-labs/FLUX.2-klein-9B` | yes (`gated: auto`) | ~34.7 GB | yes | `<side_models>/FLUX.2-klein-9B/` |
| `"4b"` | `black-forest-labs/FLUX.2-klein-4B` | **no** (apache-2.0) | ~15.98 GB | **no** | `<side_models>/FLUX.2-klein-4B/` |

Two request shapes are REFUSED with `status:"error"` rather than served with a
substitute, on both methods and before any network call:

- an unknown `variant` — never downgraded to `"9b"`, which would fetch tens of
  gigabytes the user did not ask for into a directory they did not name;
- `variant:"4b"` together with `uncensored:true` — no such repository exists,
  and handing back the official encoder under that button is a silent swap of
  the weights the user asked for.

`hf_token` is a REQUEST FIELD of both methods. It never travels as an
environment variable of the backend process, never sits inside `params`, and is
never logged on either side; a message that must mention it says "the token" and
nothing more. **A token is required only when the request touches a GATED
repository**: an empty token on a `"4b"` request is a normal request that
reaches the hub and answers `ok`, while `"9b"` (and any `uncensored:true`)
short-circuits to `no_token` with no network call.

`download.check` reports one entry per repository the variant and the
`uncensored` toggle actually need — the variant's official one always, the
uncensored encoder only while the toggle is on — and ECHOES `variant` back, so a
client can tell a fresh answer from one computed for the variant it was showing
a moment ago. `state` is one of, and the mapping is FIXED:

| state | when |
|---|---|
| `ok` | the token may download that repository |
| `no_token` | `hf_token` was empty AND the request needs one — no network call is made at all |
| `invalid_token` | HTTP 401 |
| `not_accepted` | HTTP 403 / `GatedRepoError` — the conditions were not accepted |
| `not_found` | HTTP 404 |
| `network_error` | anything else; `message` carries the detail verbatim |

`message` is always present and is empty for `ok`. The access probe is
`HfApi.auth_check`, NOT a repository listing: the metadata of a `gated: auto`
repository is public and comes back for a request with no token at all, so a
listing proves nothing about access.

**`plan` is NULLABLE and a zeroed plan is never synthesised.** It is computed from
the listings the check fetches once every needed repository answered `ok`, so the
button can say how many gigabytes are missing without a second round trip;
`missing_bytes` counts only files absent or the wrong length on disk. When no plan
could be computed, `plan` is `null`:

- the listing FAILED although every auth probe passed — `plan_error` then carries
  the message (scrubbed of the token). Access and listing are two different
  network operations, and this case deliberately gets no repo state of its own:
  auth genuinely succeeded, and calling it `network_error` would send the user to
  the wrong link. A zero-valued plan beside `ok` states is indistinguishable from
  "everything is already downloaded" and was observed rendering a complete
  installation on an empty machine, so the client must render a `null` plan as
  "size unknown", never as complete;
- no token, or a repository the user cannot download from — `plan_error` is then
  EMPTY, because the repo `state` already says what is wrong.

`plan_error` is empty whenever `plan` is present.

`download.start` streams the same envelope as `.prompt_cache.build`, so it claims
the same single progress bar and can never run beside a generation. Its progress
frames keep the four existing fields and ADD three optional ones:

```json
{ "phase": "download", "step": 12884901888, "total": 34722790808,
  "label": "transformer/diffusion_pytorch_model-00001-of-00002.safetensors",
  "file_step": 3221225472, "file_total": 9801069272,
  "file_label": "diffusion_pytorch_model-00001-of-00002.safetensors" }
```

- `step`/`total` are OVERALL bytes across the files still to fetch, which keeps
  every existing single-level consumer correct;
- `file_step`/`file_total`/`file_label` describe the file in flight. A consumer
  that does not know them ignores them, and a frame that omits them (the terminal
  one) is legal;
- frames are throttled to at most ~10 per second AND at least 4 MiB apart, and
  the last frame of a run always reports the completed byte count;
- **a staged file is published only after its length matches the size the listing
  announced for it**, and a file already on disk counts as present only at that
  same length. A short body that ends CLEANLY raises nothing anywhere in the
  stack, so without the check it is renamed to its final name and skipped by
  every later run — a permanently truncated multi-gigabyte shard, surfacing as
  corrupt weights at load time. When the listing announces no size the old
  non-empty rule stands and the reason is logged;
- `uncensored` SELECTS the text encoder, it does not add one: the plan carries the
  official `text_encoder/` or the uncensored `text_encoder_uncensored/`, never
  both, so a 9B plan is ~34.7 GB either way. Everything else comes from the
  official repository in both states, which is why both repositories are
  access-checked whenever the toggle is on. The toggle exists for `"9b"` only;
- **neither variant fetches the repository-root single-file transformer**
  (`flux-2-klein-9b.safetensors` / `flux-2-klein-4b.safetensors`): it is the same
  weights as the `transformer/` folder the plan already takes, so fetching it
  would put a duplicate 7.75-18.16 GB copy on disk, and diffusers' single-file
  loader silently discards the `device_map` this engine's layered offload needs;
- an INTERRUPTED file resumes rather than restarts: its partial bytes are parked
  beside the destination and continued with `Range: bytes=<n>-` on the next
  `download.start`, so a dropped connection at 8 of 9.8 GB does not cost those
  gigabytes again. A resumed file whose final length is still wrong is refetched
  from zero once, and only then reported as an error;
- `step` and `file_step` of a resumed file both begin at the resumed offset, so a
  client deriving a transfer rate from consecutive frames never sees a backwards
  jump. The ONE exception is that from-zero refetch, where the file genuinely
  restarts — a client must tolerate a single backwards step per file rather than
  render it as a negative or absurd rate;
- files already present at the announced size are skipped, so a re-run resumes at
  file granularity and flipping the toggle downloads only the missing encoder. Both
  encoder directories may end up on disk from two runs; the unselected one is
  never re-fetched and never deleted;
- free space is checked BEFORE the first byte and a refusal names both the
  required and the available number of bytes;
- `cancel{id}` really stops the transfer: the cancel event is polled at every
  chunk boundary. Files already published stay (they are complete and were
  renamed atomically); the staging file of the one in flight is discarded.

`params` keys (all optional except `transformer_path` and `vae_path`;
out-of-range numbers are clamped, an unknown enum or a missing/absent
transformer or VAE path is a request error — `text_encoder_path` is optional,
see "generating without a text encoder" below):
`text_encoder_path`, `transformer_path`, `vae_path`, `prompt`,
`steps` (1..50, default 4), `guidance_scale` (1.0..10.0, default 1.0 — see
"guidance and distilled checkpoints" below; on a distilled checkpoint it is
accepted and then ignored),
`strength` (0.25..1.0, default 1.0), `seed: int|null`,
`placement` (`full_gpu` | `encoder_cpu` | `model_cpu_offload` |
`sequential_cpu_offload`), `dtype` (`bfloat16` | `float16`),
`low_cpu_mem_usage: bool`, `vae_tiling: bool`, `vae_slicing: bool`,
`unload_transformer_before_vae: bool` (default: `true` for every placement
except `full_gpu`), `unload_text_encoder_after_encode: bool` (default `false`
in every placement), `text_encoder_fp8: bool` (default `false`),
`mask_dilate_px` (0..64, default 16), `mask_feather_px` (0..32, default 12),
`color_match: bool` (default `true`), `whole_region: bool` (default `false`),
`max_sequence_length` (64..512, default 512).

Region constraints, enforced by the backend and never fixed up silently: both
sides a multiple of **16** and at least **128 px**, area at most **1048576 px²**,
aspect ratio at most **8:1**. The mask must be an L8 PNG of exactly the region's
size — mode `L` is checked and any other mode (`RGB`, `RGBA`, `P`, …) is a
request error, never converted: guessing which channel carries the permission to
edit would turn a client bug into an edit of the wrong pixels. A violation is a
`status:"error"` naming the offending numbers or the mode that arrived.

**`reference_len` — the user's marks as a separate reference.** Optional; absent
or `0` means the request carries no reference. When present, the blob carries a
THIRD segment after the mask: the user's marks composited over a copy of the
region, an RGB(A) PNG (alpha is dropped) of EXACTLY the region's size —
any other size is a request error. The three lengths must sum to the blob length
exactly, like the two without it. The backend hands it to diffusers'
`Flux2KleinInpaintPipeline` as `image_reference` (one PIL image), a condition
image beside the region the pipeline already conditions on; the region itself
stays the clean image the model edits, and the reference never enters the
composite. The pipeline preprocesses a reference with the region's own rules (the
1 MP cap and the floor to a multiple of 16), so a reference of the region's size
stays pixel-aligned with it — and for a valid region both rules are no-ops. The
pre-load memory guard counts the reference's condition tokens in the denoise
phase; `.estimate` takes no reference and forecasts a run without one.

**`whole_region` — editing without a painted mask.** The request format does not
fork: the blob still carries a mask, and under `whole_region: true` it must be
SOLID (every pixel non-zero). The backend verifies that instead of trusting the
flag and answers `status:"error"` naming how many pixels are empty, because a
flag that disagrees with the data would otherwise surface as a partial edit the
user was told not to expect. The mode also settles two other params by itself:
`mask_dilate_px` becomes `0` (a full mask has nothing to grow into) and
`color_match` becomes `false` (the match takes its statistics from the pixels
OUTSIDE the mask, and here there are none — matching against the changed pixels
would force the edit's own tone back onto the original's). `mask_feather_px`
stays in force and ramps inwards from the region's border, which is what joins
the regenerated region to the rest of the page.

Response detail — `applied` is the set of memory settings ACTUALLY in force when
the run finished, `{unload_transformer_before_vae: bool, vae_tiling: bool,
vae_slicing: bool}`. The backend splits denoising from the VAE decode, so an
out-of-memory failure in the decode is recovered from without repeating the
denoise — every retry starts from the same host copy of the latents. Three rungs,
in order: it parks the transformer on the host; it enables VAE tiling, but only
when tiling would ACTUALLY engage for this region (diffusers takes the tiled
branch only above the VAE's own threshold — 1024 output px on the shipped klein
VAE — so below it the flag changes nothing and `applied.vae_tiling` must not
claim a saving that never happened); and finally it shrinks the VAE's tile itself
for ONE decode, lowering the latent and the sample threshold together, so that
tiling engages on a region the shipped threshold ignores. That last rung is an
emergency, not a setting: both thresholds are restored before the answer is sent
and neither appears in `applied`, and its output is not bit-identical to an
untiled decode because the tiles are blended. `vae_slicing` is NOT a rung at all:
`decode` slices only a batch larger than one and this service always decodes a
batch of one. When a retry was needed `oom_recovered` is `true` and `applied`
names the settings the client should persist so the next run takes the cheap path
immediately.

**FLUX.2 klein per-component residency.** The three WEIGHT-BEARING entries of
`status.components` (`text_encoder`, `transformer`, `vae` — not `tokenizer` /
`scheduler`, which carry none) also carry two fields beside their disk facts:

- `residency`: `"not_loaded"` (not in the service) | `"ram"` (every parameter and
  buffer on the host) | `"gpu"` (all of them on the compute device) |
  `"offloaded"` (an accelerate hook owns it: the parameters sit on `meta` and the
  bytes in a host weights map) | `"mixed"` (genuinely split across devices).
  Three labels are not enough to be honest: under `sequential_cpu_offload` the
  component is in neither RAM nor VRAM, and a load can leave part of one behind.
  `"mixed"` is REPORTED, never rounded to a neighbouring state.
- `actions`: which of `load` / `unload` / `to_ram` / `to_gpu` / `warmup` are
  possible RIGHT NOW. This list is the AUTHORITY — the client renders it and must
  never re-derive it, or the matrix would exist twice and drift on the first
  edit. Invariants the backend holds: the text encoder never offers `to_gpu`
  (prompt encoding pins the host and the load-order memory contract depends on
  it); the transformer never offers `warmup` (the warm-up forwards the VAE only,
  and no transformer path exists); `load` and `unload` appear on BOTH the
  transformer and the VAE, always together, because they act on the pipeline as
  a whole — `_model_key` describes a whole pipeline, so one of them alone can
  neither be dropped nor loaded. The VAE is never moved by hand: no helper exists
  and a host-resident VAE would silently make the next decode run on the CPU.

Both fields are ABSENT — and `components_busy` is `true` — when the backend could
not take its service lock without waiting. A generation holds that lock for its
entire run, so a blocking probe would hang the handler for minutes. **An absent
field means "not known", never "not loaded"**: the same three-state rule the
client already applies to `prompt_cached` and `text_encoder_available`. The rest
of `status` is answered regardless; nothing else in it takes the lock.

`inpaint.flux2_klein.component_action` performs ONE of those actions. It is
STREAMING (`phase:"load"`, the same 0..9 scale) because reading the encoder takes
tens of seconds and the transformer 18 GB, and it claims the same single progress bar a
generation does — so an action and a generation can never run at once. `params`
is the same normalized object every other FLUX.2 call takes. Refusals are
`status:"error"` with an actionable message, never a silent no-op: the action is
not in that component's current `actions`, the service is busy, or the pre-load
memory guard refuses — a user-pressed load of the 16 GB encoder or a 9B restore
to the card goes through the SAME guard, with the same reserves and the same
"what does fit" advice, that a generation does.

**FLUX.2 klein streaming detail**: same frame shape as FLUX.1, two phases:

```json
{ "v":1, "id":42, "kind":"progress", "phase":"load", "step":3, "total":9, "label":"Загрузка VAE" }
```
- `phase:"load"` — `step`/`total` (0..9) follow the run's preparation in order:
  the transformer, tokenizer, VAE and scheduler are loaded and placed (1-5), the
  placed weights are warmed up (6), and only then is the text encoder read and
  the prompt encoded (7-9). That order is a memory contract, not a cosmetic
  choice: the encoder's 16 GB arrive into the host memory the transformer has
  just vacated for the accelerator. The warm-up is a `load` step and never a
  `generate` one.
- `phase:"generate"` — `step`/`total` are diffusion steps (`total` already
  accounts for the steps `strength` drops).

**FLUX.2 klein prompt cache.** Encoding a prompt costs a ~16 GB read of the Qwen3
encoder (tens of seconds) and yields ~4 MiB of
embeddings, so embeddings are cached in memory and can be kept on disk. The
in-memory cache, `prompt_cache.build`, `prompt_cache.load` and `status`'s
`prompt_cached` all use ONE key: prompt text + encoder path +
`max_sequence_length` + `dtype` + `text_encoder_fp8`.

- `prompt_cached` in `.status` is `true` only when a READY embedding exists for
  exactly that combination. An empty prompt, an unset path or an unknown enum
  answers `false` without an error — the UI polls this while the user types.
- `prompt_cache.build` encodes the prompt and NOTHING else: no transformer, no
  VAE, no image. It streams the same `phase:"load"` frames as a run, but only the
  prompt phase's steps occur (0, then 7-9 of the same 0..9 scale), because no
  pipeline is built. The memory guard therefore checks only the standalone encode
  and never demands room for the 9B transformer. `encoded: false` means the cache
  already covered the prompt and nothing was read. The encoder is released
  afterwards unless one was already resident with the same settings.
- **The library** lives at `<program root>/prompt_cache/<encoder family>/<name>.msprompt`.
  A family directory is `<encoder folder name>-<8 hex of the encoder fingerprint>`
  — readable, and unique even though every checkout names its folder
  `text_encoder`. Family and entry names are sanitized to a single path
  component, so neither a user-chosen name nor a hand-made file can write outside
  the library. An existing entry name is refused unless `overwrite: true`:
  rebuilding a lost entry costs the encoder read it was saved to avoid. Every
  write is atomic (`<name>.<pid>.part` + `fsync` + `os.replace`).
- **`.msprompt`** is a safetensors container with one `prompt_embeds` tensor and
  a string `__metadata__` map: `format`
  (`manhwastudio.flux2_klein.prompt_cache`), `format_version`, the ORIGINAL
  `prompt`, `max_sequence_length`, `dtype`, `text_encoder_fp8`,
  `text_encoder_id`, `text_encoder_family`, `text_encoder_path` (informational
  only) and `created_at`. `text_encoder_id` is a SHA-256 of the encoder's
  `config.json` plus the sorted `(file name, size)` list of its weight files —
  cheap on purpose: hashing the 16 GB would cost more than re-encoding.
- **A file that does not match is refused, never "loaded to see".** `.load`
  answers `status:"error"` naming the field that disagrees: another encoder,
  another `max_sequence_length`, another dtype (including a metadata that
  disagrees with the tensor's own dtype token), another fp8 setting, a container
  without our format marker, or a newer `format_version`. Foreign embeddings
  would load silently and denoise into a different picture — a wrong result with
  no error attached.
- **`.import` files an entry under the family recorded IN THE FILE**, not under
  the currently selected encoder, so an imported cache is never lost among
  another encoder's entries. A mismatch is not an error: the answer carries
  `family_matches: false` (with `current_family`) for the client to warn about.
  `.load` of a foreign entry stays a hard error — importing files a cache away,
  loading feeds a generation.
- `.list` returns `entries` (`name`, `family`, `prompt`, `created_at`,
  `size_bytes`, `max_sequence_length`, `dtype`) plus `skipped` (`name`, `reason`,
  and `family` in a library-wide listing): a corrupt or foreign file in the
  directory is reported, never fatal.
- The blob is always empty; this method never streams a preview.

**Generating without a text encoder.** `text_encoder_path` is OPTIONAL: an
embedding is all a run needs, and the four denoising steps and the VAE decode
never touch the encoder. A `.msprompt` copied to a machine where the 16 GB Qwen3
was never downloaded therefore works — the client sends no encoder path (or one
that no longer exists, which is what a settings file carried over from another
machine looks like; both mean the same thing), calls `prompt_cache.load` and then
generates.

- The absence is an error only where a prompt actually has to be ENCODED: a
  generation whose prompt is not in the cache, and `prompt_cache.build`. Both
  answer `status:"error"` before anything is loaded, naming both ways out —
  configure an encoder, or load a ready cache for this prompt. A path that exists
  but carries no `config.json` stays a hard error rather than degrading into "no
  encoder".
- **What `.load` still checks with no encoder present**: the format marker, the
  format version, `max_sequence_length`, the dtype (metadata AND the tensor's own
  dtype token) and the fp8 flag. Only the fingerprint comparison is skipped, and
  the answer says which happened in `encoder_verified` — `true` when the
  fingerprint was compared, `false` when the file's own metadata was taken on
  trust. Clients should surface the difference rather than hide it.
- **The library has no current family without an encoder**, because the family
  name is derived from the encoder. `.list` then spans EVERY family: each entry
  names its own `family`, the top-level `family` is `""` (none is active) and
  `directory` is the library root. `.load` and `.export` look a name up across
  families; a name present in two of them is a `status:"error"` naming both
  rather than an arbitrary choice. `.save` is the one library method that still
  REQUIRES an encoder — the entry it writes has to name the encoder that produced
  it.
- `.status` reports `text_encoder_available` next to `available`. `available` is
  not `false` merely because the encoder is missing when `prompt_cached` is
  `true`; the client is expected to warn from `text_encoder_available: false`
  that only ready caches will work. Only the encoder is optional — the
  `tokenizer/` and `scheduler/` directories are pipeline components and are still
  required.
- `.estimate` is lower on such a machine: the encode phase cannot run there, so
  it contributes nothing to `peak_encode` or to the totals. The same single
  forecast still gates the load.

**FLUX.2 klein: guidance and distilled checkpoints.** `.status` answers
`guidance_supported: bool` — whether `guidance_scale` can do anything at all on
the configured checkpoint. It is `false` exactly when the checkpoint's own
`model_index.json` declares `"is_distilled": true`, which the shipped klein
builds do: guidance was distilled into such a model, so diffusers computes
`do_classifier_free_guidance = guidance_scale > 1 and not is_distilled` as
`false` and a raised slider buys nothing while costing twice the compute per
denoise step. The backend therefore pins the scale to `1.0` and encodes no
negative prompt on such a checkpoint, however the request was filled in; the
request is NOT refused, and `guidance_scale` stays a valid param. A checkpoint
that declares nothing — a hand-assembled folder of components, which carries no
`model_index.json` — reports `guidance_supported: true` and behaves exactly as
before: the slider is live and classifier-free guidance really runs. The field is
a plain boolean, never a third "unknown" value: it says what the run will do, and
the run treats an undeclared checkpoint as guided. Clients should use it to
disable the control, not to decide whether to send the param.

All inpaint engines require Torch.

### 5.5 Device

| HTTP path                    | method                     | request fields (inline)                                                            | blob(req) | response fields (status=ok)                                                                                                                                                                                                                                                                                 | blob(resp) | stream | cancel |
|------------------------------|----------------------------|------------------------------------------------------------------------------------|-----------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------|--------|--------|
| GET /device                  | `device.get`               | (none)                                                                              | none      | `selected_device: string`, `available_devices: string[]`, `available_device_options: object[]`, `torch_device_needs_selection: bool`, `max_loaded_models: int`, `selected_onnx_provider: string`, `available_onnx_providers: string[]`, `selected_onnx_device_id: string`, `available_onnx_device_options: object[]`, `available_onnx_devices_by_provider: object`, `onnx_device_needs_selection: bool` | none | no | no |
| POST /device/set             | `device.set`               | `device: string\|null`, `onnx_provider: string\|null`, `onnx_device_id: string\|null`, `max_loaded_models: int\|null` | none | same fields as `device.get` (status=ok)                                                                                                                                                                                                                                                                    | none       | no     | no     |
| GET /device/cuda_diagnostics | `device.cuda_diagnostics`  | (none)                                                                              | none      | `diagnostics: object`                                                                                                                                                                                                                                                                                        | none       | no     | no     |

Notes:
- `device.set` accepts any subset of the four fields; absent/`null` fields are
  left unchanged. Returns the full new device state (identical shape to
  `device.get`).
- Device state changes SHOULD also be pushed via the `device` event topic (§7).

### 5.6 Reline

| HTTP path           | method            | request fields (inline)                                                                 | blob(req) | response fields (status=ok)                                                                  | blob(resp) | stream | cancel |
|---------------------|-------------------|-----------------------------------------------------------------------------------------|-----------|----------------------------------------------------------------------------------------------|------------|--------|--------|
| GET /reline/models  | `reline.models`   | (none)                                                                                   | none      | `models: object[]` (each `{name, filename, downloaded}`)                                      | none       | no     | no     |
| POST /reline/process | `reline.process` | `image_path: string` (required, on-disk), `output_path: string\|null`, `params: object` (RelineOptions) | none | full Reline service result object passed through verbatim (includes at least `ok`); output written to `output_path` on disk | none | no | no |

Notes:
- Reline operates on **on-disk paths**, not blobs: `image_path` in, processed PNG
  written to `output_path`. No image bytes cross the socket.
- `params` mirrors the Rust `RelineOptions` (`reader_mode`, `upscale`, `sharp`,
  `halftone`, `resize`, `level`, `cvt_color`); `upscale.enabled == true`
  requires Torch.
- `reline.process` returns the backend service result object verbatim (the HTTP
  endpoint forwards `result` unchanged).

### 5.7 Health (pull form)

| HTTP path   | method   | request fields | blob(req) | response fields (status=ok)                                                                                                                                                  | blob(resp) | stream | cancel |
|-------------|----------|----------------|-----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------|--------|--------|
| GET /health | `health` | (none)         | none      | the health snapshot object (see §7 `health` topic for the shape)                                                                                                            | none       | no     | no     |

Health is primarily delivered via the `health` event topic; this request/response
form lets a freshly connected client pull the current snapshot once.

### 5.8 Browser (advanced web scraping)

| method            | request fields (inline)      | blob(req) | response fields (status=ok)                 | blob(resp) | stream  | cancel |
|-------------------|------------------------------|-----------|-----------------------------------------------|------------|---------|--------|
| `browser.command` | `payload: object` (required) | none      | the daemon's terminal event dict, inline      | none       | **yes** | partial |

Notes:
- `payload` is the advanced-download command object, e.g.
  `{"command":"open_url","browser":"chrome","url":"https://..."}`. The set of
  commands and their fields is owned by the daemon classes under
  `modules/new_project/`, not by this protocol: `browser.command` is one method
  that tunnels all of them.
- The response header is the daemon's single terminal event
  (`opened` / `result` / `auto_result` / `link_collect_started` /
  `intercept_count` / …) with fields such as `current_url`, `output_dir`,
  `downloaded_images`, `found_links`, `found_pages`, `items`.
- Daemon `progress` events are relayed as `progress{id}` frames. There is never
  a response blob: downloaded images stay on disk and are handed over as
  `output_dir` + a count.
- Cancellation is partial: only commands whose long work polls a cancel file
  (auto-fetch / deep-intercept) honor `cancel{id}`; the rest run to completion.

### 5.9 Visible watermark removal

Domain `watermark.*`, backed by `AppState.watermark`
(`modules/ai_backend/watermark/service.py`). `watermark.detect` is the primary
flow — the predicted mask is handed to the existing inpaint engines — and
`watermark.remove` is the experimental direct network pass.

| method              | request fields (inline)                                                  | blob(req)       | response fields (status=ok)                                                                                                             | blob(resp)             | stream  | cancel |
|---------------------|--------------------------------------------------------------------------|-----------------|-------------------------------------------------------------------------------------------------------------------------------------------|------------------------|---------|--------|
| `watermark.detect`  | `params: object={}` (`model, downscale_to, threshold, dilate_px`)         | input image PNG | `model: string`, `device: string`, `source_size: [w,h]`, `mask_coverage: float`                                                              | L8 mask PNG            | **yes** | yes    |
| `watermark.remove`  | `params: object={}` (`model, tile, overlap, …`)                          | input image PNG | `model: string`, `device: string`, `source_size: [w,h]`, `image_len: int`, `mask_len: int`                                                   | clean PNG ++ mask PNG  | **yes** | yes    |
| `watermark.status`  | (none)                                                                    | none            | `models: object[]` (each `{id, weights_ready, code_ready}`), `default_model: string`, `downloaded_models: string[]`, `code_ready_models: string[]` | none                   | no      | no     |
| `watermark.unload`  | (none)                                                                    | none            | `unloaded: bool`                                                                                                                            | none                   | no      | no     |

Notes:
- The request blob must be non-empty: unlike the text detectors (§5.3) there is
  no on-disk `page_path` alternative for these methods.
- `watermark.remove` is the one method that returns **two** images. It reuses the
  §5.4 length-prefixed-appendix convention on the RESPONSE side: the response
  blob is `clean_png ++ mask_png` and `image_len` + `mask_len` split it, with
  `image_len + mask_len == blob_len` enforced by strict equality. Neither image
  is base64-encoded, and neither travels in the header.
- `params` is optional; `null` means "absent" and any non-object value is a
  request error. Its keys are owned by the service, not by this protocol.
- `watermark.detect` and `watermark.remove` require Torch. `watermark.status`
  and `watermark.unload` do not (status only inspects the disk).
- Weights and the network code are downloaded on demand into
  `ManhwaStudio_AI_Models/side_models/WatermarkRemoval/`, so a first call can
  include a large download — hence the `download` progress phase below and a
  generous client-side timeout.

**Watermark streaming detail**: identical to the FLUX two-phase contract — a
`phase` header field, `step`/`total`, a human `label`, and never a blob:

```json
{ "v":1, "id":42, "kind":"progress", "phase":"download", "step":1048576, "total":85672345, "label":"model_best.pth.tar" }
```
- `phase:"download"` — `step`/`total` are BYTES, `label` names the file.
- `phase:"generate"` — `step`/`total` are tiles (or detection passes) processed,
  `label` names the current tile row.

---

## 6. Error model

There are two distinct failure channels:

1. **Request failure** (the method ran but failed, or input was invalid): the
   server sends a terminal `response` with `status:"error"` and an `error`
   string, echoing the request `id`:

   ```json
   { "v":1, "id":42, "kind":"response", "status":"error",
     "error": "Field 'texts' must not be empty." }
   ```

   Cancellation is a request outcome too: `status:"interrupted"` (with an
   optional `error` describing the interruption). This maps the legacy
   HTTP 400/500 + `{"ok":false,"error":...}` and the OCR 409-interrupt payload.

2. **Protocol error** (framing/parse/version/size-guard violation, unknown
   `kind`/`method`): the server sends a `kind:"error"` frame. If it can be
   attributed to a request it echoes that `id`; otherwise `id` is 0 and the
   connection is typically closed:

   ```json
   { "v":1, "id":0, "kind":"error", "error": "Unknown method: ocr.bogus" }
   ```

**Torch gate:** methods marked "requires Torch" (manga `base_torch`, easy,
paddle_vl, surya, all inpaint, `watermark.detect`/`watermark.remove`,
ctd/surya detectors, reline upscale) fail with a
`response{status:"error"}` whose `error` is the Torch-unavailable message when
Torch is absent or disabled. This replaces the legacy HTTP 503.

**Version mismatch:** during `hello`, if the client's `v` != server
`PROTOCOL_VERSION`, the server replies with a `kind:"error"` frame (id 0) and
closes the connection. The client treats it as a fatal, clean handshake failure
and does not send requests.

---

## 7. Event topics

Events are `kind:"event"`, `id:0`, with a `topic`. Payload fields are inline in
`header_json` unless noted.

| topic        | trigger                                    | payload (inline header fields)                                                                                                                                                                                                  | blob              |
|--------------|--------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------|
| `health`     | periodic (~1s) snapshot push               | `ok: bool`, `service: "mf_ai_backend"`, `backend_version: string`, `snapshot_unix_s: float`, `is_torch_available: bool`, `ocr: {easyocr, mangaocr, paddleocr, paddleocrvl, suryaocr}`, `text_detector: {ctd, paddle, surya}`, `inpaint: {lama_v2, lama_mpe, aot, flux_fill}`, `watermark: object`, `image_processing: {reline}`, `machine_translation: object`, `model_manager: object`. (Warming-up form: `snapshot_state:"warming_up"` plus `ok/service/backend_version/snapshot_unix_s/is_torch_available`.) | none              |
| `device`     | device/provider selection changed          | the full `device.get` result shape (selected/available torch + onnx fields)                                                                                                                                                    | none              |
| `model_load` | model load/unload progress (new in v2)     | `model: string`, `phase: string` (e.g. `"start"\|"progress"\|"done"\|"unload"`), `loaded: int` (optional), `total: int` (optional), `message: string` (optional)                                                               | none              |
| `log`        | optional backend log line stream (opt-in)  | `level: string` (e.g. `"info"\|"warn"\|"error"`), `message: string`, `ts_unix_s: float` (optional)                                                                                                                             | none              |

The `health` snapshot payload mirrors `_build_health_snapshot` /
`_get_health_snapshot` in `server.py` exactly (each nested service value is that
service's own `.health()` object, passed through). The `health` event replaces
the Rust client polling `GET /health`.

---

## 8. Historical note

The framed IPC (this protocol) was introduced alongside the legacy HTTP backend
during a migration phase. That migration is complete: the HTTP backend and the
dual-socket arrangement have been removed. This framed protocol is now the only
application-level protocol between the Rust frontend and the Python AI backend.
It rides over a pluggable transport — AF_UNIX by default, with a loopback
WebSocket carrier added as a fallback on Windows (where CPython lacks
`socket.AF_UNIX`).
