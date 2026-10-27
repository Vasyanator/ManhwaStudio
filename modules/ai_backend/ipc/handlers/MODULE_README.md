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
- `ocr.py`: `ocr.manga` / `.easy` / `.paddle` / `.paddle_vl` / `.surya` / `.paddle_onnx` /
  `.baberu` (`paddle_onnx` routes through the same `state.paddle_ocr` service as `ocr.paddle`).
  The model-location fields Rust sends (`paddle_vl_model` + `paddle_vl_model_dir`,
  `easyocr_model_dir`, `surya_recognition_dir` + `surya_detection_dir`, and the
  `baberu_model_files` object) are REQUIRED; this layer checks only their presence and type and
  forwards them verbatim — the label syntax and every path check are the service's single answer.
  `ocr.baberu` is not Torch-gated (ONNX Runtime: vision on the selected EP, decoders on CPU).
- `textdetector.py`: `textdetector.ctd.forward` / `.paddle.forward` / `.surya.forward` — forward-only
  detection. Owns the wire contract (`FORWARD_SPECS`: align, map stride, channel names per engine;
  `parse_forward_request` / `encode_forward_response`) and validates BOTH directions: `n`, tile
  alignment, exact blob length, `MAX_BLOB_BYTES` for the request and the response it implies, and
  the service's map shape/dtype. Services receive `uint8 [n, h, w, 3]` and return `uint8` maps;
  Surya also requires `surya_detection_dir`, forwarded as the service's `model_dir`. numpy is
  imported inside the handler. The table is mirrored by `ms_backend_ipc::textdetector`.
- `inpaint.py`: `inpaint.lama_v2` / `.lama_mpe` / `.aot` and their `.unload` methods.
- `sdxl.py`: `inpaint.sdxl` (+ `.unload`) — streaming, with a latent-preview PNG blob per `progress`.
  Requires `sdxl_config_dir` and forwards the optional `sdxl_model_dir` (type checks only; the
  service resolves the model source, `PROTOCOL.md §5.4`).
- `flux_fill.py`: `inpaint.flux_fill` (+ `.unload`, `.status`) — streaming `download` and `generate`
  phases, no preview blob.
- `flux2_klein.py`: `inpaint.flux2_klein` (+ `.status`, `.unload`,
  `.component_action` and the four `.prompt_cache.*` methods) — FLUX.2
  klein region editing. Generation streams `load`/`generate` phases (never `download`: by then the
  weights are user-supplied paths) and returns `image_len` + the
  OOM-recovery report (`oom_recovered`, `applied`) and the four step keys in the response header.
  Its request blob is
  `region ++ mask [++ reference]`, split by `_split_request_blob` with strict equality against
  `image_len + mask_len + reference_len`; an absent or zero `reference_len` reaches the service as
  `reference_bytes=None`. Forecasts are `large_models.profiles` (`large_models.py`), never a
  FLUX.2 method. **`applied` carries exactly the names of `_APPLIED_FLAGS`** (`vae_tiling`), each
  always present: the client persists it after a run. The retired placement fields are refused
  by the service's params normalization, not here.
  `prompt_cache.build` is the second streaming method of the group (same
  `{phase, step, total, label}` frames through the shared `streaming.progress_forwarder`, no blob): it
  saves a named preset into the model's prompt library, encoding without generating anything when
  needed, and reading the text encoder takes tens of seconds, so a silent wait is not acceptable.
  `prompt_cache.list` / `.export` / `.import` are plain request/response (protocol 23; `.save` /
  `.load` are gone); their bodies are `prompt_cache_handlers.py`'s. The preset `name` (optional,
  `optional_str`), entry `file` and `path` are forwarded VERBATIM — `require_non_empty_str` only checks that a required field arrived as a
  non-empty string; what makes a path, a file name or a name acceptable is the service's business
  (`prompt_library.require_entry_source` / `require_entry_file_name` / `normalize_preset_name`),
  and duplicating those rules here would give two answers to one question.
  `component_action` is the THIRD streaming method of the group and follows the same rule:
  `component` and `action` are forwarded verbatim, because which names are legal and which are
  possible for a component right now is the service's single answer (`ACTIONABLE_COMPONENTS`,
  `COMPONENT_ACTIONS`, `_require_action_available_locked`) — the per-component action matrix must
  exist in exactly one place or it drifts on the first edit. See `PROTOCOL.md §5.4`
  ("per-component residency") and `dev-docs/flux2_component_residency.md`.
  The group downloads nothing: every FLUX.2 klein weight comes from the app's model catalog. The
  load phase's byte level (`file_step` / `file_total` / `file_label`) is
  ADDITIVE: `streaming.progress_forwarder` takes them (and `file_component`, the component a FLUX.2
  load reads, and `stage`) as keyword-only optionals and omits each one it
  was not given, so `step`/`total` keep meaning the overall level and every four-positional-argument
  caller emits exactly the frame it always did. See `PROTOCOL.md`.
- `flux3_klein.py`: `inpaint.flux3_klein` (+ `.status`, `.unload` and the four `.prompt_cache.*`
  methods) — FLUX.3 klein region editing over `ctx.state.flux3_klein`. The run's blob
  is `region ++ mask ++ references` split by `image_len` / `mask_len` / `reference_lens` (0..9
  positive lengths, strict sum; `MAX_REFERENCES` mirrors `wire.MAX_EXTRA_REFERENCES`); `params` is
  forwarded verbatim to `inpaint/flux3_klein/region.py` (whose `wire.py` is the one params owner).
  The run streams `load` / `generate` frames with a stable `stage` and is canceled MID-RUN:
  `cancel_event.is_set` is the region editor's `should_cancel` (checked at every stage begin and
  denoise step), `Flux3Canceled` becomes `Interrupted`. The group downloads nothing: the FLUX.3
  klein package comes from the app's model catalog, and the former `.download.check` /
  `.download.start` are unknown methods since protocol 24. The `.prompt_cache.*` bodies are
  `prompt_cache_handlers.py`'s; the build passes `_RegionPromptBuild` as the service, which runs
  `region.build_prompt_cache` (the run's frame builder: stages `prepare` / `encoder_load` /
  `encode` / `encoder_release`, the cancel checked at every stage begin too).
- `streaming.py`: helpers shared by the region-editor groups (`flux2_klein.py`, `flux3_klein.py`),
  registering no method: `require_segment_len`, `require_params` and `progress_forwarder` (a dead peer
  never aborts the work; the first failed frame of a request is logged once).
  Not imported by `__init__.py` (it is not a group).
- `prompt_cache_handlers.py`: the bodies of the four `<model>.prompt_cache.*` methods
  (`handle_build` streaming, `handle_list`, `handle_export`, `handle_import`) and the header readers
  `require_non_empty_str` / `optional_str`, registering no method: a group registers its own method
  names and passes its service (duck-typed `prompt_cache_build(params, *, name, progress_callback)`,
  `prompt_cache_list` / `_export` / `_import`) and its method name (the cancel texts and the progress
  frames name it). Used by `flux2_klein.py` and `flux3_klein.py`. Not imported by `__init__.py` (it is not a group).
- `watermark.py`: `watermark.detect` / `.remove` / `.status` / `.unload` — visible-watermark removal
  (`ctx.state.watermark`). Same two-phase streaming contract as `flux_fill.py`; `.remove` is the only
  method whose RESPONSE blob concatenates two PNGs (`clean ++ mask`, split by `image_len`/`mask_len`).
- `large_models.py`: `large_models.profiles` / `.limits.set` / `.status` / `.unload` /
  `.learned.reset` over `ctx.state.large_models` (`LargeModelManager`). Validates field types and
  builds the model-agnostic `RequestShape` and the requested `Steps` (`vram_step` / `ram_step`,
  each `auto` when absent); what a step, a format or a limit means is the manager's answer, and
  `profiles` answers `ProfilesReport.to_dict()` unchanged. The retired single-scale fields
  (`profile` on `profiles`, `keep_policy` on `limits.set`) are refused by name. `profiles` turns the model's wire `params` into the manager's options and device
  through `ctx.state.large_model_adapters[model].large_model_inputs(params) -> (options, device)`
  (the model service implements it; a model without an adapter is refused); an adapter whose
  params select another registration adds `large_model_name(params) -> str`. An adapter whose
  operation opens several handles (FLUX.2: pipeline + encoder) adds
  `large_model_profiles(params, shape, *, format, steps) -> dict` and composes the whole
  `to_dict()`-shaped answer itself (e.g. through `LargeModelManager.profiles(...,
  claimed_device_bytes=, claimed_host_bytes=)`); when present it is preferred. `limits.set`: an absent
  field keeps its value, `null` clears it, merged atomically by `LargeModelManager.set_limits`. Imports only the stdlib-only `runtime/large_models/spec`
  and `tiers` modules (torch-free). See `../PROTOCOL.md §5.10`.
- `reline.py`: `reline.models` / `reline.process` (on-disk paths only, no image bytes).
- `device.py`: `device.get` / `.set` / `.cuda_diagnostics`.
- `translate.py`: `translate.deep`.
- `browser.py`: `browser.command` — the whole advanced-download surface behind one method.
- `test_<group>.py`: one per handler module. They drive handlers with a `SimpleNamespace`/`MagicMock`
  stand-in for `AppState` and need no torch, no model, and no socket.

## Contracts and invariants
- A handler never constructs a service and never imports `server.py` or a service package. It reads
  `ctx.state.<field>`; those `AppState` field names are the cross-layer contract with `server.py`.
  There is exactly ONE documented exception, a stateless helper with no `AppState` counterpart,
  imported INSIDE a function so this package stays torch-free at import time: `sdxl.py`'s
  `_encode_png_bytes_rgb` from `../../inpaint/sdxl.py` (a pure byte helper). Do not grow that list:
  a helper that acquires state belongs on `AppState`.
  (`large_models.py` importing the pure value types `RequestShape` / `VramStep` / `RamStep` / `Steps`
  from `runtime/large_models/` is not a service import: those modules are stdlib-only and hold no
  state.)
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
  where the work is an unbounded network transfer or a long multi-phase run the event itself must
  reach the work (the region editors' runs hand `cancel_event.is_set` in as `should_cancel`), or
  the method is cancellable only on paper.
- Raw bytes only: request/response blobs carry PNG bytes, never base64. Two-image inpaint requests
  arrive as one concatenated blob split by the `image_len`/`mask_len` header fields; the same
  convention is used in the response direction by `watermark.remove`.

## Editing map
- Change an existing method's wire shape: that group's module, and `../PROTOCOL.md §5` first.
- Add a method to an existing group: the group module + a `METHOD_*` constant in `../protocol.py`.
- Add a new group: see the contract above; `__init__.py` is the only shared file you touch.
- Change how progress frames are emitted or cancelled: `../dispatcher.py`, not here.
