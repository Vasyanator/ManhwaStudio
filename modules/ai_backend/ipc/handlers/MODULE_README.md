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
- `textdetector.py`: `textdetector.ctd.forward` / `.paddle.forward` / `.surya.forward` — forward-only
  detection. Owns the wire contract (`FORWARD_SPECS`: align, map stride, channel names per engine;
  `parse_forward_request` / `encode_forward_response`) and validates BOTH directions: `n`, tile
  alignment, exact blob length, `MAX_BLOB_BYTES` for the request and the response it implies, and
  the service's map shape/dtype. Services receive `uint8 [n, h, w, 3]` and return `uint8` maps;
  numpy is imported inside the handler. The table is mirrored by `ms_backend_ipc::textdetector`.
- `inpaint.py`: `inpaint.lama_v2` / `.lama_mpe` / `.aot` and their `.unload` methods.
- `sdxl.py`: `inpaint.sdxl` (+ `.unload`) — streaming, with a latent-preview PNG blob per `progress`.
- `flux_fill.py`: `inpaint.flux_fill` (+ `.unload`, `.status`) — streaming `download` and `generate`
  phases, no preview blob.
- `flux2_klein.py`: `inpaint.flux2_klein` (+ `.status`, `.estimate`, `.unload`,
  `.component_action`, the six `.prompt_cache.*` methods and the two `.download.*` methods) — FLUX.2
  klein region editing. Generation streams `load`/`generate` phases (never `download`: by then the
  weights are user-supplied paths) and returns `image_len` + the
  OOM-recovery report (`oom_recovered`, `applied`) in the response header. `.estimate` is the only
  inpaint method taking a region size instead of image bytes.
  **`applied` must always carry all five names of `_APPLIED_FLAGS`**: the Rust client parses it as
  one struct and ignores an incomplete object outright, so omitting a key does not degrade the
  answer — it throws the whole OOM-recovery report away.
  `prompt_cache.build` is the second streaming method of the group (same
  `{phase, step, total, label}` frames through the shared `_progress_forwarder`, no blob): it
  encodes a prompt without generating anything, and reading the text encoder takes tens of seconds,
  so a
  silent wait is not acceptable. `prompt_cache.list`/`.save`/`.load`/`.export`/`.import` are plain
  request/response. Names and paths are forwarded VERBATIM — `_require_non_empty_str` only checks
  that the field arrived as a non-empty string; what makes a path or a name acceptable is the
  service's business (`require_prompt_file_source`, `sanitize_name_component`), and duplicating
  those rules here would give two answers to one question.
  `component_action` is the THIRD streaming method of the group and follows the same rule:
  `component` and `action` are forwarded verbatim, because which names are legal and which are
  possible for a component right now is the service's single answer (`ACTIONABLE_COMPONENTS`,
  `COMPONENT_ACTIONS`, `_require_action_available_locked`) — the per-component action matrix must
  exist in exactly one place or it drifts on the first edit. See `PROTOCOL.md §5.4`
  ("per-component residency") and `dev-docs/flux2_component_residency.md`.
  `download.check` / `download.start` acquire the klein weights from Hugging Face and are the
  group's two exceptions to the shape above, both deliberate:
  * they reach `../../inpaint/flux2_download.py` through a lazy import (`_download_module`)
    instead of a `ctx.state` field, because that module is stateless — no resident weights, no
    lease, no residency — so it is not an `AppState` service. The precedent is `sdxl.py`'s lazy
    `from ...inpaint.sdxl import …`. Adding a service field for it would put an empty object in
    the composition root just to satisfy a shape.
  * `download.start` is the ONE method of this group that observes cancellation INSIDE the work:
    `cancel_event.is_set` is handed to the downloader, which polls it at every chunk boundary.
    Everywhere else the shared inpaint rule ("checked before the call and after it returns, never
    inside a weight read") holds, but a cancel that only stopped the progress frames while the
    socket kept pulling gigabytes would be a lie.
  `download.check`'s `plan` is NULLABLE and its `plan_error` travels beside it; this layer forwards
  both unchanged. A listing failure is NOT turned into a repo state here or anywhere else — auth
  succeeded, and the client renders a `null` plan as "size unknown", never as a finished install.
  The token arrives as the `hf_token` REQUEST FIELD (`_read_hf_token`), never as an environment
  variable and never inside `params`; nothing in this layer logs it, echoes it back or names it in
  an error. The `variant` field (`_read_variant`) is forwarded VERBATIM for the same reason
  `component` and `action` are: which variants exist, that an absent field means `"9b"`, that an
  unknown name is a refusal and that `"4b"` has no uncensored encoder are the downloader's single
  answer (`resolve_variant` / `require_uncensored_supported`). Re-deriving any of it here is the
  drift the contract puts it in one place to avoid — and note that an EMPTY `hf_token` is a legal,
  servable request for `"4b"`, whose repository is public, so this layer must not gate on it. The download's second progress level (`file_step` / `file_total` / `file_label`) is
  ADDITIVE: `_progress_forwarder` takes the three as keyword-only optionals and omits each one it
  was not given, so `step`/`total` keep meaning the overall level and every four-positional-argument
  caller emits exactly the frame it always did. See `PROTOCOL.md` and
  `dev-docs/flux2_model_download.md`.
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
  There are exactly TWO documented exceptions, both stateless helpers with no `AppState`
  counterpart and both imported INSIDE a function so this package stays torch-free at import time:
  `sdxl.py`'s `_encode_png_bytes_rgb` from `../../inpaint/sdxl.py` (a pure byte helper), and
  `flux2_klein.py`'s `_download_module()` -> `../../inpaint/flux2_download.py` (a function module
  that owns no resident state, so a service field for it would be an empty object in the composition
  root). Do not grow that list: a helper that acquires state belongs on `AppState`.
- This package must stay importable without torch, diffusers, onnxruntime or any model. Heavy work
  belongs to the service; a module-level import of a service package here would break the IPC tests
  and the torch-free `ipc/` guarantee.
- Registration happens at module level, exactly once per method. A method registered twice, or
  registered but absent from `protocol.py`'s method list, is a defect.
- Adding a group means: create `handlers/<group>.py`, add ONE import line to `__init__.py`, add the
  `METHOD_*` constant to `../protocol.py`, and document the method in `../PROTOCOL.md §5`.
- Long work must observe `cancel_event` and raise `Interrupted` so the dispatcher can answer
  `response{status:"interrupted"}`; a handler that ignores it makes the method uncancellable.
  Checking it only around the service call is enough where the work is a bounded weight read;
  where the work is an unbounded network transfer the event itself must reach the work
  (`inpaint.flux2_klein.download.start` is the one method that does this today), or the method is
  cancellable only on paper.
- Raw bytes only: request/response blobs carry PNG bytes, never base64. Two-image inpaint requests
  arrive as one concatenated blob split by the `image_len`/`mask_len` header fields; the same
  convention is used in the response direction by `watermark.remove`.

## Editing map
- Change an existing method's wire shape: that group's module, and `../PROTOCOL.md §5` first.
- Add a method to an existing group: the group module + a `METHOD_*` constant in `../protocol.py`.
- Add a new group: see the contract above; `__init__.py` is the only shared file you touch.
- Change how progress frames are emitted or cancelled: `../dispatcher.py`, not here.
