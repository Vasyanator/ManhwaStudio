# Module: modules/ai_backend/ipc/handlers

## Purpose
The IPC method surface of the backend: one module per feature group, each turning a `request` frame
into a call on a service published on `AppState` and back into a `(header, blob)` response. This is
the ONLY layer that knows the wire shape of a method — services know nothing about frames, and the
transport knows nothing about methods.

The authoritative per-method request/response spec is `../PROTOCOL.md §5`; this file describes the
layer's structure and rules.

## Architecture
Every module registers its handlers at import time via `registry.register(METHOD_X, _handle_x)` (or
the `@register(METHOD_X)` decorator form), so importing this package is what wires the whole method
table. `registry.py` performs that import once; nothing else needs to.

A handler has the fixed signature
`(ctx: HandlerContext, header: dict, blob: bytes, cancel_event: threading.Event) -> (dict, bytes)`.
It reaches services only through `ctx.state.<AppState field>`, streams intermediate frames through
`ctx.progress_emitter` (present only for streaming methods), and raises `Interrupted` when
`cancel_event` is set.

## Files and submodules
- `__init__.py`: the single shared touch-point — one import line per group, and the instructions for
  adding a new one. Never add handler imports to `registry.py` instead.
- `health.py`: `health` (`ctx.get_health_snapshot`, not a service call).
- `ocr.py`: `ocr.manga` / `.easy` / `.paddle` / `.paddle_vl` / `.surya` / `.paddle_onnx`
  (`paddle_onnx` routes through the same `state.paddle_ocr` service as `ocr.paddle`).
- `textdetector.py`: `textdetector.ctd` / `.paddle` / `.surya`; owns the `mask_png` header/blob split.
- `inpaint.py`: `inpaint.lama_v2` / `.lama_mpe` / `.aot` and their `.unload` methods.
- `sdxl.py`: `inpaint.sdxl` (+ `.unload`) — streaming, with a latent-preview PNG blob per `progress`.
- `flux_fill.py`: `inpaint.flux_fill` (+ `.unload`, `.status`) — streaming `download` and `generate`
  phases, no preview blob.
- `flux2_klein.py`: `inpaint.flux2_klein` (+ `.status`, `.estimate`, `.unload`, and the six
  `.prompt_cache.*` methods) — FLUX.2 klein region editing. Streams `load`/`generate` phases
  (never `download`: the weights are user-supplied paths) and returns `image_len` + the
  OOM-recovery report (`oom_recovered`, `applied`) in the response header. `.estimate` is the only
  inpaint method taking a region size instead of image bytes.
  **`applied` must always carry all five names of `_APPLIED_FLAGS`**: the Rust client parses it as
  one struct and ignores an incomplete object outright, so omitting a key does not degrade the
  answer — it throws the whole OOM-recovery report away.
  `prompt_cache.build` is the second streaming method of the group (same
  `{phase, step, total, label}` frames through the shared `_progress_forwarder`, no blob): it
  encodes a prompt without generating anything, and reading the text encoder takes ~106 s, so a
  silent wait is not acceptable. `prompt_cache.list`/`.save`/`.load`/`.export`/`.import` are plain
  request/response. Names and paths are forwarded VERBATIM — `_require_non_empty_str` only checks
  that the field arrived as a non-empty string; what makes a path or a name acceptable is the
  service's business (`require_prompt_file_source`, `sanitize_name_component`), and duplicating
  those rules here would give two answers to one question.
- `watermark.py`: `watermark.detect` / `.remove` / `.status` / `.unload` — visible-watermark removal
  (`ctx.state.watermark`). Same two-phase streaming contract as `flux_fill.py`; `.remove` is the only
  method whose RESPONSE blob concatenates two PNGs (`clean ++ mask`, split by `image_len`/`mask_len`).
- `reline.py`: `reline.models` / `reline.process` (on-disk paths only, no image bytes).
- `device.py`: `device.get` / `.set` / `.cuda_diagnostics`.
- `translate.py`: `translate.deep`.
- `browser.py`: `browser.command` — the whole advanced-download surface behind one method.
- `test_<group>.py`: one per handler module. They drive handlers with a `SimpleNamespace`/`MagicMock`
  stand-in for `AppState` and need no torch, no model, and no socket.

## Contracts and invariants
- A handler never constructs a service and never imports `server.py` or a service package. It reads
  `ctx.state.<field>`; those `AppState` field names are the cross-layer contract with `server.py`.
  The ONE documented exception is `sdxl.py`, which lazily imports `_encode_png_bytes_rgb` from
  `../../inpaint/sdxl.py` inside its progress callback (a pure byte helper with no `AppState`
  counterpart). The import stays inside the function so this package remains torch-free at import
  time. Do not grow that exception.
- This package must stay importable without torch, diffusers, onnxruntime or any model. Heavy work
  belongs to the service; a module-level import of a service package here would break the IPC tests
  and the torch-free `ipc/` guarantee.
- Registration happens at module level, exactly once per method. A method registered twice, or
  registered but absent from `protocol.py`'s method list, is a defect.
- Adding a group means: create `handlers/<group>.py`, add ONE import line to `__init__.py`, add the
  `METHOD_*` constant to `../protocol.py`, and document the method in `../PROTOCOL.md §5`.
- Long work must observe `cancel_event` and raise `Interrupted` so the dispatcher can answer
  `response{status:"interrupted"}`; a handler that ignores it makes the method uncancellable.
- Raw bytes only: request/response blobs carry PNG bytes, never base64. Two-image inpaint requests
  arrive as one concatenated blob split by the `image_len`/`mask_len` header fields; the same
  convention is used in the response direction by `watermark.remove`.

## Editing map
- Change an existing method's wire shape: that group's module, and `../PROTOCOL.md §5` first.
- Add a method to an existing group: the group module + a `METHOD_*` constant in `../protocol.py`.
- Add a new group: see the contract above; `__init__.py` is the only shared file you touch.
- Change how progress frames are emitted or cancelled: `../dispatcher.py`, not here.
