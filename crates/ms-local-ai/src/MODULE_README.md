# Module: crates/ms-local-ai/src

## Purpose
The process's LOCAL AI facts, GUI-free: which local AI runtimes this process may use and the one
rule that maps an AI route onto "can run here" (`local_ai`), and the push-driven health model of
the Python AI backend with its device-control probe thread (`backend_health`).

## Architecture
Consumers: the Translation tab (OCR / text-detector routing, health gating), the Cleaning tab
(offline message, health snapshot), `ms-settings-ui` (AI backend pane, supervisor, settings
warnings), the launcher's reline flow and the root (`LocalAiAvailability::for_process` in
`src/startup/services.rs`, the device prompt). None of them needs a tab crate for these facts.

Dependencies: `ms-backend-ipc` (all backend calls via `shared_client()`), `ms-config`
(`AiRuntime`), `ms-platform` (capability table), `ms-sysprobe` (`ai_backend_capabilities`
slots), `ms-log`, `ms-thread`, `ms-i18n`. Layer: the `ms-project / ms-widgets /
ms-native-runtime` line (`ARCHITECTURE.md` §2).

## Files and submodules
- `lib.rs`: crate root, mounts the i18n macros.
- `local_ai.rs`: `LocalAiAvailability` (may this process use the Python backend / native ONNX:
  `--no-ai` switches both off, the platform capability table decides independently, a platform
  reason wins; built once at startup), `LocalAiOff` / `LocalRuntime` / `LocalAiOffReason` (typed
  reason + localized `user_message`), `AiRouteKind` and its `availability` (the ONE
  route-availability rule: cloud always, native needs native ONNX allowed, backend needs the
  backend allowed), and `effective_ai_runtime` (the router's runtime: a stored `Backend` reads as
  `Native` without `PlatformFeature::PythonBackend`).
- `backend_health.rs`: push-driven backend health via `TOPIC_HEALTH` v2 events (with a one-shot
  `health` pull as the startup/liveness fallback, `check_ai_backend_health`), plus `device`
  get/set, ONNX provider, max-loaded-models and CUDA diagnostics helpers
  (`AiBackendProbeCommand`, `spawn_ai_backend_probe`), and `ai_backend_offline_error`. The
  snapshot's `backend_version` is DIAGNOSTIC only — never compared with the program version, never
  gates a feature; compatibility is decided by `PROTOCOL_VERSION` in the `hello` handshake. It also
  carries the per-engine OCR readiness flags (`ocr.<engine>.ready`, incl. `baberuocr`) and the
  resident PaddleOCR-VL variant (`ocr.paddleocrvl.model`); readiness of the SELECTED variant is
  `paddle_vl_ready_for`, so a variant switch never shows a stale "ready".

## Contracts and invariants
- `--no-ai` switches off every LOCAL runtime (backend AND native ONNX); cloud AI API and plain
  machine translators are never gated by it. A platform reason wins over the flag.
- Every route-availability decision goes through `AiRouteKind::availability`; routers read the
  runtime through `effective_ai_runtime`, never `AiRuntime::from_user_settings` directly.
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
