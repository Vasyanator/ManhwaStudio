# Module: crates/ms-local-ai/src

## Purpose
The process's LOCAL AI facts, GUI-free: which local AI runtimes this process may use and the one
rule that maps an AI route onto "can run here" (`local_ai`), and the push-driven health model of
the Python AI backend with its device-control probe thread (`backend_health`), and the one Rust
client of the backend's large-model pool (`large_models`).

## Architecture
Consumers: the Translation tab (OCR / text-detector routing, health gating), the Cleaning tab
(offline message, health snapshot), `ms-settings-ui` (AI backend pane, supervisor, settings
warnings), the launcher's reline flow and the root (`LocalAiAvailability::for_process` +
`install_process` in `src/startup/services.rs`, the device prompt). None of them needs a tab crate for these facts.

Consumers of `large_models`: the Cleaning tab's large-model engines (FLUX.2 klein) and the AI
backend settings pane's «Большие модели» section.

Dependencies: `ms-backend-ipc` (all backend calls via `shared_client()`), `ms-config`
(`AiRuntime`), `ms-platform` (capability table), `ms-sysprobe` (`ai_backend_capabilities`
slots), `ms-log`, `ms-thread`, `ms-i18n`. Layer: the `ms-project / ms-widgets /
ms-native-runtime` line (`ARCHITECTURE.md` §2).

## Files and submodules
- `lib.rs`: crate root, mounts the i18n macros.
- `large_models.rs`: the `large_models.*` client (protocol v13, `PROTOCOL.md` §5.10): the step
  vocabulary (`VramStep` / `RamStep` behind `LargeModelStep`, wire ids + localized labels and
  hints), the answer types and their STRICT parsers (`parse_profiles`, `parse_pool_status`,
  `parse_limits`: an unknown step id or a missing required field is `Malformed`, never a silent
  zero), the backend machine codes as typed enums (`RefusalReason`, `FailureKind`,
  `SuggestionKind`, `Resource`; `ms-i18n` resolves only static keys), the blocking calls
  (`fetch_profiles`, `set_limits`, `unload`, `reset_learned` — WORKER THREADS ONLY), the shared
  formatters `format_gib` / `format_duration`, and the two GUI-safe helpers: `ProfilesQuery`
  (one request in flight, latest wins, a superseded answer kept as stale) and `LimitsWriter`
  (debounced latest-wins `limits.set`, AXIS-SCOPED: an untouched axis travels as `Keep` so two
  editors never overwrite each other; a pending edit is sent on a worker when the writer drops). Both take an injectable worker function for tests. The
  pinned answer shape is `../fixtures/large_models_profiles_v13.json` (also read by the Python
  contract test). A question's `ProfilesRequest::references` (`[w, h]` of every reference image,
  FLUX.3 klein: the region first) travel as `shape.references` only when non-empty, so a model
  without references (FLUX.2) sends the shape it always did; they are part of `ProfilesKey`.
  The answer's FLUX.2-only additive keys `encoder` (the text encoder's plan) and
  `encoder_formats` (its weight-format rows, the `formats` row shape) read as `None` / empty
  when absent or `null`, and are parsed strictly when present. A forecast's optional
  `host_baseline` (the baseline RSS its `host_peak` includes) reads as `None` when absent; it
  lets a consumer add two forecasts' host peaks without counting the process twice.
- `local_ai.rs`: `LocalAiAvailability` (may this process use the Python backend / native ONNX /
  the Stable-Diffusion.cpp worker: `--no-ai` switches all three off, the platform capability
  table decides independently, a platform reason wins; built once at startup), the process-global
  copy of it (`install_process`, set once by the root's `init_full`, a second install is logged
  and ignored; `process()` reads it, `for_process(false)` before or without an install),
  `LocalAiOff` / `LocalRuntime` / `LocalAiOffReason` (typed reason + localized `user_message`),
  `AiRouteKind` and its `availability` (the ONE route-availability rule: cloud always, native
  needs native ONNX allowed, backend needs the backend allowed, sd.cpp needs sd.cpp allowed), and `effective_ai_runtime` (the router's runtime: a stored `Backend` reads as
  `Native` without `PlatformFeature::PythonBackend`).
- `backend_health.rs`: push-driven backend health via `TOPIC_HEALTH` v2 events (with a one-shot
  `health` pull as the startup/liveness fallback, `check_ai_backend_health`), plus `device`
  get/set, ONNX provider, max-loaded-models and CUDA diagnostics helpers
  (`AiBackendProbeCommand`, `spawn_ai_backend_probe`), and `ai_backend_offline_error`. The
  snapshot's `backend_version` is DIAGNOSTIC only — never compared with the program version, never
  gates a feature; compatibility is decided by `PROTOCOL_VERSION` in the `hello` handshake. It also
  carries the per-engine OCR readiness flags (`ocr.<engine>.ready`, incl. `baberuocr`) and the
  resident PaddleOCR-VL variant (`ocr.paddleocrvl.model`); readiness of the SELECTED variant is
  `paddle_vl_ready_for`, so a variant switch never shows a stale "ready". The large-model pool
  part of the pushed `model_manager` payload lands in `AiBackendHealthSnapshot::large_pool`
  (parsed by `large_models::parse_pool_status`; `None` when not reported). The snapshot's
  `torch` object (protocol v18) lands in `AiBackendHealthSnapshot::torch_build` as
  `TorchBuildInfo` (flavour from `ms_config::torch_choice::TorchFlavour`, versions, visible GPUs
  with `GfxArch`, installed per-arch ROCm kernel wheels), strictly parsed: `null` / absent is
  `None`, an unknown flavour token or a bad shape is a typed `TorchBuildError`.
  `TorchBuildInfo::kernel_mismatch` is the one "main GPU's arch has no installed ROCm kernels"
  rule (primary device = first discrete, else first; only for the split-kernel whl-next layout,
  `rocm_split_kernels`, where an empty wheel list means no kernels; silent for a pytorch.org
  ROCm build with bundled kernels).

## Contracts and invariants
- `large_models` mirrors only the backend's wire ids; step SEMANTICS are
  `runtime/large_models/tiers.py`'s and are never re-derived here. The pool caps have one
  writer, the backend (`limits.set` -> `runtime/device_service.py`); no Rust code writes
  `General.large_models_*_cap_bytes`.
- `--no-ai` switches off every LOCAL runtime (backend, native ONNX AND sd.cpp); cloud AI API and plain
  machine translators are never gated by it. A platform reason wins over the flag.
- Every route-availability decision goes through `AiRouteKind::availability`; routers read the
  runtime through `effective_ai_runtime`, never `AiRuntime::from_user_settings` directly.
- `torch_build` is diagnostic only (the AI backend pane): nothing gates a feature on it, and the
  flavour / arch vocabulary is `ms_config::torch_choice`'s, never redefined here.
- `backend_health` owns no endpoint/socket state; transport is `ms_backend_ipc`. The probe runs on
  its own thread; nothing here runs on the GUI thread or blocks it.
- The i18n keys keep their `translation.backend_health.*` / `translation.local_ai.*` names
  (catalog keys are stable ids, not module paths).
- No egui, no tab state; this crate must never depend on a tab crate, `ms-settings-ui`,
  `ms-launcher` or the root package.

## Editing map
- A new local runtime or a change of the `--no-ai` / platform rule: `local_ai.rs`
  (`LocalAiAvailability::resolve`, `AiRouteKind::availability`).
- A new health field or device-control command: `backend_health.rs` (`HealthFields`,
  `apply_health_fields`, `AiBackendProbeCommand`).
- A `large_models.*` shape change: `large_models.rs` (parser + its test) and the fixture, then
  the consumers (`ms-tab-cleaning` `ai_editor::large_model_resources`, `ms-settings-ui`
  `large_models_panel`).
