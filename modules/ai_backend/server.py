"""
File: modules/ai_backend/server.py

Purpose:
Composition root of the Python AI backend: it builds every local AI service and
serves them over the framed IPC protocol used by the Rust application.

This is the ONLY module that knows the full set of domain sub-packages
(`runtime/`, `engines/`, `ocr/`, `detection/`, `inpaint/`, `watermark/`,
`reline/`, `translate/`, `browser/`) and wires their services into the shared
`AppState`.
Everything downstream (the IPC handlers in `ipc/handlers/`) reaches a service
only through `HandlerContext.state.<attr>`, so the `AppState` field names are a
cross-layer contract: renaming one silently breaks a handler and therefore an
IPC method.

Main responsibilities:
- construct the shared `AppState` (OCR / text detector / inpaint / translation /
  device services) consumed by the IPC handlers in `ipc/handlers/`
  (`build_app_state`): ONE `ResidencyArbiter` over ONE `MemoryBudgetProbes`, the
  small-model pool `SmallModelManager(arbiter=...)` and the large-model pool
  `LargeModelManager(arbiter, ...)` over the same arbiter and probes, its limits
  persisted in `user_config` through `AiDeviceService` and its learned floors in
  `<model cache root>/large_models/limits.json`; the FLUX.2 klein service is the
  `large_models.profiles` adapter of `flux2_klein` (`large_model_adapters`);
- apply the Linux OOM backstop (`General.large_models_oom_score_adj`) at start;
- maintain a non-blocking health snapshot and publish it as a `health` event on
  the IPC event bus;
- expose the backend version metadata for Rust-side compatibility checks.

Transport:
The backend speaks the framed, multiplexed IPC protocol over one of two byte
transports, selected by `run_server(transport=...)`:
- `"unix"` (default): a single AF_UNIX domain socket via `ipc/frame_server.py`,
  enforcing a single live instance via stale-socket detection and unlinking the
  socket file on shutdown. AF_UNIX works on Linux and Windows 10 1803+.
- `"ws"`: a token-authenticated WebSocket server via `ipc/frame_ws_server.py`
  (Python is the WS server, Rust the WS client), used where AF_UNIX is
  unavailable. See `ipc/frame_ws_server.py` and `ipc/PROTOCOL.md`.
Both transports run the SAME dispatcher/handler stack; only the byte transport
differs.
"""

from __future__ import annotations

import functools
import os
import socket
import threading
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from collections.abc import Mapping
from typing import Any

# Domain sub-packages wired together here (see the module docstring): each import
# names the package that owns the service, never a flat top-level module.
from .runtime.device_service import AiDeviceService
from .runtime.model_manager import SmallModelManager
from .runtime.large_models.cost_tables import CostTables, detect_device_class, load_tables
from .runtime.large_models.manager import JsonLimitsStore, LargeModelManager, apply_oom_score_backstop, limits_store_path
from .runtime.memory_budget import TorchDeviceProbe
from .runtime.residency.arbiter import MemoryBudgetProbes, ResidencyArbiter
from .runtime.torch_support import is_torch_available
from .engines.paddle_onnx import RuntimeFactory, resolve_compiled_cache_root
from .detection.ctd import CtdTextDetectorService
from .detection.paddle import PaddleTextDetectorService
from .detection.surya import SuryaTextDetectorService
from .ocr.baberu import BaberuOcrService
from .ocr.easy import EasyOcrService
from .ocr.manga import MangaOcrService
from .ocr.paddle import PaddleOcrService
from .ocr.paddle_vl import PaddleVlOcrService
from .ocr.surya import SuryaOcrService
from .inpaint.aot import AotInpaintService
from .inpaint.lama import LamaInpaintService
from .inpaint.lama_mpe import LamaMpeInpaintService
from .inpaint.sdxl import SdxlInpaintService
from .inpaint.flux_fill import FluxFillInpaintService
from .inpaint.flux2_klein import Flux2KleinInpaintService
from .watermark.service import WatermarkRemovalService
from .reline.service import RelineService
from .translate.machine_translation import MachineTranslationService
from .browser.service import BrowserService
from .ipc.protocol import TOPIC_HEALTH

HEALTH_SNAPSHOT_REFRESH_SECS = 1.0

# ============================================================================
# AI BACKEND SERVER — composition root
# ----------------------------------------------------------------------------
# What lives here:
# - `AppState`: the shared OCR / MT / inpaint / text-detector / device services.
#   Its field names are the contract the IPC handlers depend on via
#   `HandlerContext.state.<attr>`; do not rename them without updating
#   `ipc/handlers/` and the Rust-side method expectations.
# - `_build_health_snapshot` / `_health_snapshot_worker`: background health
#   snapshot, also published as a `health` event on the IPC bus so clients never
#   need to poll.
# - `run_server`: constructs the services and starts the framed IPC server on
#   the selected transport. Request routing lives in `ipc/handlers/`, not here.
# ============================================================================

@dataclass
class AppState:
    """Shared, process-wide service container handed to every IPC handler.

    Built once by `run_server` and reached by handlers through
    `HandlerContext.state`. Field names are part of the cross-layer contract
    (see the module docstring): each one backs at least one IPC method.
    """

    app_version: str
    model_manager: SmallModelManager
    easy_ocr: EasyOcrService
    manga_ocr: MangaOcrService
    paddle_ocr: PaddleOcrService
    paddle_vl_ocr: PaddleVlOcrService
    surya_ocr: SuryaOcrService
    baberu_ocr: BaberuOcrService
    text_detector_ctd: CtdTextDetectorService
    text_detector_paddle: PaddleTextDetectorService
    text_detector_surya: SuryaTextDetectorService
    lama_inpaint: LamaInpaintService
    lama_mpe_inpaint: LamaMpeInpaintService
    aot_inpaint: AotInpaintService
    sdxl_inpaint: SdxlInpaintService
    flux_fill_inpaint: FluxFillInpaintService
    flux2_klein_inpaint: Flux2KleinInpaintService
    watermark: WatermarkRemovalService
    reline: RelineService
    machine_translation: MachineTranslationService
    ai_device: AiDeviceService
    browser: BrowserService
    # The large-model pool (`large_models.*` IPC, FLUX services), sharing the
    # small pool's arbiter and memory probes.
    large_models: LargeModelManager
    # `large_models.profiles` adapters: {model name registered with `large_models`:
    # an object with `large_model_inputs(params: dict) -> (options, device)`} (the
    # contract is `ipc/handlers/large_models.py`).
    large_model_adapters: Mapping[str, Any]
    # Probes of the health snapshot's `model_manager.budget` only: no warm-up
    # matmul, so the health thread never synchronizes a device behind a running
    # inference (the arbiter's probes warm up; these must not).
    health_probes: MemoryBudgetProbes
    health_snapshot: dict[str, Any] = field(default_factory=dict)
    health_snapshot_lock: threading.Lock = field(default_factory=threading.Lock, repr=False)


def _safe_service_health(service: Any) -> dict[str, Any]:
    """Call `service.health()` but never let one failing service kill the snapshot.

    `_build_health_snapshot` aggregates every service's `.health()`; one of those
    can still raise (e.g. a filesystem or optional-package probe it runs, such as
    the Surya detector's checkpoint lookup). If any single
    call threw, the whole snapshot build would throw and the periodic `health`
    event would never be published. We isolate each sub-entry: a raising/missing
    service yields a `{"status":"error","error":...}` placeholder instead, keeping
    the snapshot's overall shape and the event pipeline alive.
    """
    try:
        return service.health()
    except Exception as exc:  # noqa: BLE001 - one bad service must not sink the rest
        return {"status": "error", "error": str(exc)}


def _build_health_snapshot(state: AppState) -> dict[str, Any]:
    now_s = time.time()
    return {
        "ok": True,
        "service": "mf_ai_backend",
        "backend_version": state.app_version,
        "snapshot_unix_s": now_s,
        "is_torch_available": is_torch_available(),
        "ocr": {
            "easyocr": _safe_service_health(state.easy_ocr),
            "mangaocr": _safe_service_health(state.manga_ocr),
            "paddleocr": _safe_service_health(state.paddle_ocr),
            "paddleocrvl": _safe_service_health(state.paddle_vl_ocr),
            "suryaocr": _safe_service_health(state.surya_ocr),
            "baberuocr": _safe_service_health(state.baberu_ocr),
        },
        "text_detector": {
            "ctd": _safe_service_health(state.text_detector_ctd),
            "paddle": _safe_service_health(state.text_detector_paddle),
            "surya": _safe_service_health(state.text_detector_surya),
        },
        "inpaint": {
            "lama_v2": _safe_service_health(state.lama_inpaint),
            "lama_mpe": _safe_service_health(state.lama_mpe_inpaint),
            "aot": _safe_service_health(state.aot_inpaint),
            "flux_fill": _safe_service_health(state.flux_fill_inpaint),
            "flux2_klein": _safe_service_health(state.flux2_klein_inpaint),
        },
        # Watermark removal is a domain of its own, not an inpaint engine: it
        # predicts a mask instead of consuming one, so it gets a top-level key.
        "watermark": _safe_service_health(state.watermark),
        "image_processing": {
            "reline": _safe_service_health(state.reline),
        },
        "machine_translation": _safe_service_health(state.machine_translation),
        "model_manager": _model_manager_health(state),
    }


def _model_manager_health(state: AppState) -> dict[str, Any]:
    """The `model_manager` health object: the small pool's four counters plus the additive keys of v13.

    Additive keys, each built in isolation (a failing one becomes the
    `{"status":"error","error":...}` placeholder, the others survive):
    `small_models` (one entry per small-pool holding), `large_models` + `limits`
    (`LargeModelManager.health()`), `budget` (`_budget_health`). The four
    counters keep the old all-or-nothing behaviour of `_safe_service_health`.
    """
    payload = _safe_service_health(state.model_manager)
    payload = dict(payload)
    try:
        payload["small_models"] = [_small_holding_entry(view) for view in state.model_manager.holdings().values()]
    except Exception as exc:  # noqa: BLE001 - one bad part must not sink the snapshot (FIX-6)
        payload["small_models"] = {"status": "error", "error": str(exc)}
    large = _safe_service_health(state.large_models)
    if "large_models" in large:
        payload.update(large)
    else:
        payload["large_models"] = large
    try:
        payload["budget"] = _budget_health(state, payload)
    except Exception as exc:  # noqa: BLE001 - a probe failure must not sink the snapshot (FIX-6)
        payload["budget"] = {"status": "error", "error": str(exc)}
    return payload


def _small_holding_entry(view: Any) -> dict[str, Any]:
    """One `small_models` entry from a residency `HoldingView`."""
    footprint = view.footprint
    return {
        "key": view.key,
        "owner": view.owner,
        "device": footprint.device,
        "device_bytes": footprint.device_bytes,
        "host_bytes": footprint.host_bytes,
        "size_source": footprint.source.value,
        "resident": view.resident,
        "in_use": view.in_use_count > 0,
        "loading": view.loading,
    }


def _budget_health(state: AppState, model_manager_payload: Mapping[str, Any]) -> dict[str, Any]:
    """`model_manager.budget`: host budget, the budgets of devices that hold or reserve bytes, outstanding reservations.

    Only `cuda*` devices named by a holding or a reservation are probed, so the
    snapshot never initializes a GPU context the backend did not already use.
    """
    reservations = state.model_manager.arbiter.health()["reservations"]
    devices: set[str] = set(reservations["device_bytes"])
    for entry in model_manager_payload.get("small_models") or ():
        if isinstance(entry, dict) and entry.get("device"):
            devices.add(str(entry["device"]))
    for entry in model_manager_payload.get("large_models") or ():
        if isinstance(entry, dict) and entry.get("device") and (entry.get("device_bytes") or 0) > 0:
            devices.add(str(entry["device"]))
    host = state.health_probes.host()
    device_rows = []
    for device in sorted(name for name in devices if name.lower().startswith("cuda")):
        budget = state.health_probes.device(device)
        device_rows.append({"device": device, "total": budget.total, "free": budget.free, "headroom": budget.headroom(), "reserve": budget.reserve})
    return {
        "host": {"total": host.total, "available": host.available, "headroom": host.headroom(), "reserve": host.reserve, "cgroup_limit": host.cgroup_limit},
        "devices": device_rows,
        "reservations": reservations,
    }


def _set_health_snapshot(state: AppState, payload: dict[str, Any]) -> None:
    with state.health_snapshot_lock:
        state.health_snapshot = payload


def _get_health_snapshot(state: AppState) -> dict[str, Any]:
    with state.health_snapshot_lock:
        if state.health_snapshot:
            return dict(state.health_snapshot)
    return {
        "ok": True,
        "service": "mf_ai_backend",
        "backend_version": state.app_version,
        "snapshot_unix_s": time.time(),
        "snapshot_state": "warming_up",
        "is_torch_available": is_torch_available(),
    }


@functools.lru_cache(maxsize=None)
def _cost_tables_for(device: str) -> CostTables:
    """The large-model cost tables of `device`'s class, read once per device name.

    Raises `OSError` / `ValueError` when the tracked calibration data is missing or
    malformed (an installation defect, see `cost_tables.load_tables`); nothing is
    cached then, so the next request retries.
    """
    return load_tables(detect_device_class(device))


def build_app_state(app_version: str, *, user_config: Any = None, large_models_cache_root: Path | None = None) -> AppState:
    """Construct every backend service and the `AppState` the IPC handlers read; starts no thread and no transport.

    Wiring: one `MemoryBudgetProbes` -> one `ResidencyArbiter` -> the small pool
    `SmallModelManager(arbiter=...)` and the large pool `LargeModelManager(arbiter,
    probes=<the same probes>)`; the large pool's limits persist in `user_config`
    `General` through `AiDeviceService` and its learned floors in
    `limits_store_path(<cache root>)`. `user_config` (default: the process
    `config.UserConfig`) and `large_models_cache_root` (default:
    `engines.paddle_onnx.resolve_compiled_cache_root()`) exist so tests never touch
    the user's documents. Constructing reads the settings document and may
    normalize `General.ai_max_loaded_models` (as before); it loads no model.
    """
    probes = MemoryBudgetProbes()
    arbiter = ResidencyArbiter(probes)
    model_manager = SmallModelManager(arbiter=arbiter)
    onnx_runtime_factory = RuntimeFactory(model_manager)
    ai_device_service = AiDeviceService(model_manager, user_config=user_config)
    cache_root = large_models_cache_root if large_models_cache_root is not None else resolve_compiled_cache_root()
    large_models = LargeModelManager(
        arbiter,
        limits_store=ai_device_service.large_models_limits_store(JsonLimitsStore(limits_store_path(cache_root))),
        tables=_cost_tables_for,
        probes=probes,
    )
    # Shared so the SDXL 4-channel prefill reuses the same LaMa model cache.
    lama_inpaint_service = LamaInpaintService(model_manager)
    flux2_klein_service = Flux2KleinInpaintService(model_manager, large_models=large_models)
    return AppState(
        app_version=app_version,
        model_manager=model_manager,
        easy_ocr=EasyOcrService(model_manager),
        manga_ocr=MangaOcrService(model_manager, ai_device_service),
        paddle_ocr=PaddleOcrService(onnx_runtime_factory),
        paddle_vl_ocr=PaddleVlOcrService(model_manager),
        surya_ocr=SuryaOcrService(model_manager),
        baberu_ocr=BaberuOcrService(model_manager, ai_device_service),
        text_detector_ctd=CtdTextDetectorService(model_manager),
        text_detector_paddle=PaddleTextDetectorService(onnx_runtime_factory),
        text_detector_surya=SuryaTextDetectorService(model_manager),
        lama_inpaint=lama_inpaint_service,
        lama_mpe_inpaint=LamaMpeInpaintService(model_manager),
        aot_inpaint=AotInpaintService(model_manager),
        sdxl_inpaint=SdxlInpaintService(model_manager, lama_inpaint_service),
        flux_fill_inpaint=FluxFillInpaintService(model_manager),
        flux2_klein_inpaint=flux2_klein_service,
        watermark=WatermarkRemovalService(model_manager),
        reline=RelineService(),
        machine_translation=MachineTranslationService(),
        ai_device=ai_device_service,
        browser=BrowserService(),
        large_models=large_models,
        # A large model is servable by `large_models.profiles` when its service
        # (registered with `large_models`) is listed here under its registered
        # model name; the service implements `large_model_inputs(params)`.
        large_model_adapters={"flux2_klein": flux2_klein_service},
        health_probes=MemoryBudgetProbes(device_probe=TorchDeviceProbe(warm_up=False)),
    )


def run_server(
    *,
    socket_path: Path | str,
    warmup_mangaocr: bool = False,
    app_version: str,
    transport: str = "unix",
    ws_host: str = "127.0.0.1",
    ws_port: int = 0,
    ws_token: str | None = None,
) -> None:
    """Build all AI services and serve the framed IPC protocol over the chosen transport.

    `transport` selects the byte transport for the SAME framed, multiplexed IPC
    protocol:

    - `"unix"` (default): bind the AF_UNIX socket at `socket_path` via
      `run_frame_server`. A live backend already on that path raises
      `FrameBackendInstanceError`; a stale socket file is replaced. The frame
      server enforces single-instance ownership and `chmod 0o600` on the socket,
      runs in the foreground until interrupted, then unlinks the socket file.
      Raises RuntimeError on a Python build without AF_UNIX support.
    - `"ws"`: serve the WebSocket fallback transport via `run_frame_ws_server`,
      bound to `(ws_host, ws_port)` (`ws_port == 0` picks an ephemeral port). The
      handshake requires the `token` query param to equal `ws_token`
      (constant-time); `ws_token` must be provided. The actual bound port is
      printed as `MS_BACKEND_WS_PORT=<port>` for the Rust supervisor.

    Raises ValueError for an unknown `transport` value or a `"ws"` transport with
    no `ws_token`.
    """
    state = build_app_state(app_version)
    # Linux: make the kernel's OOM killer pick this backend, never the editor
    # (`General.large_models_oom_score_adj`, default on; never raises).
    apply_oom_score_backstop(state.ai_device.general_section())
    _set_health_snapshot(
        state,
        {
            "ok": True,
            "service": "mf_ai_backend",
            "backend_version": state.app_version,
            "snapshot_unix_s": time.time(),
            "snapshot_state": "warming_up",
            "is_torch_available": is_torch_available(),
        },
    )
    stop_event = threading.Event()

    # --- framed IPC protocol -----------------------------------------------
    # The framed, multiplexed protocol runs over the transport selected by
    # `transport`. The frame server owns the event bus; the health worker
    # publishes snapshots to it so clients receive health pushes instead of
    # polling. Request routing lives in `ipc/handlers/`, which reach the AppState
    # services directly via the handler context — identical for both transports.
    from .ipc.events import EventBus

    event_bus = EventBus()

    health_thread = threading.Thread(
        target=_health_snapshot_worker,
        args=(state, stop_event, event_bus),
        daemon=True,
    )
    health_thread.start()
    if warmup_mangaocr:
        threading.Thread(target=_warmup_safe, args=(state,), daemon=True).start()

    try:
        if transport == "unix":
            # AF_UNIX is required for the unix transport; fail loudly on a Python
            # build that lacks it instead of an obscure AttributeError at bind
            # time. This guard only applies to the unix branch — the WS transport
            # never touches AF_UNIX.
            if not hasattr(socket, "AF_UNIX"):
                raise RuntimeError(
                    "This Windows build of Python lacks AF_UNIX support; "
                    "Windows 10 1803+ with a modern CPython is required."
                )
            from .ipc.frame_server import run_frame_server

            socket_path_str = os.fspath(socket_path)
            print(f"[AI Backend] Running framed IPC on unix socket {socket_path_str}")
            run_frame_server(
                state,
                socket_path_str,
                stop_event,
                backend_version=state.app_version,
                get_health_snapshot=lambda: _get_health_snapshot(state),
                events=event_bus,
            )
        elif transport == "ws":
            if not ws_token:
                raise ValueError(
                    "transport='ws' requires a non-empty --ws-token; refusing to "
                    "serve an unauthenticated WebSocket transport."
                )
            from .ipc.frame_ws_server import run_frame_ws_server

            print(f"[AI Backend] Running framed IPC on ws://{ws_host}:{ws_port}")
            run_frame_ws_server(
                state,
                ws_host,
                ws_port,
                ws_token,
                stop_event,
                backend_version=state.app_version,
                get_health_snapshot=lambda: _get_health_snapshot(state),
                events=event_bus,
            )
        else:
            raise ValueError(
                f"Unknown IPC transport {transport!r}; expected 'unix' or 'ws'."
            )
    except KeyboardInterrupt:
        print("\n[AI Backend] Stopping...")
    finally:
        stop_event.set()
        try:
            state.browser.close()
        except Exception:  # noqa: BLE001 - browser teardown is best-effort
            traceback.print_exc()


def _warmup_safe(state: AppState) -> None:
    try:
        state.manga_ocr.warmup()
    except Exception:
        traceback.print_exc()


def _health_snapshot_worker(
    state: AppState,
    stop_event: threading.Event,
    event_bus: Any | None = None,
) -> None:
    while not stop_event.is_set():
        try:
            snapshot = _build_health_snapshot(state)
            _set_health_snapshot(state, snapshot)
            # Also push the snapshot to v2 frame clients as a `health` event so
            # they no longer need to poll. Best-effort; a publish failure (e.g.
            # a dead subscriber) must never stall the health worker.
            if event_bus is not None:
                try:
                    event_bus.publish(TOPIC_HEALTH, snapshot)
                except Exception:
                    traceback.print_exc()
        except Exception:
            traceback.print_exc()
        stop_event.wait(HEALTH_SNAPSHOT_REFRESH_SECS)
