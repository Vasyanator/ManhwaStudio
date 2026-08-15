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
  device services) consumed by the IPC handlers in `ipc/handlers/`;
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

import os
import socket
import threading
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Domain sub-packages wired together here (see the module docstring): each import
# names the package that owns the service, never a flat top-level module.
from .runtime.device_service import AiDeviceService
from .runtime.model_manager import LoadedModelManager
from .runtime.torch_support import is_torch_available
from .engines.paddle_onnx import RuntimeFactory
from .detection.ctd import CtdTextDetectorService
from .detection.paddle import PaddleTextDetectorService
from .detection.surya import SuryaTextDetectorService
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
    model_manager: LoadedModelManager
    easy_ocr: EasyOcrService
    manga_ocr: MangaOcrService
    paddle_ocr: PaddleOcrService
    paddle_vl_ocr: PaddleVlOcrService
    surya_ocr: SuryaOcrService
    text_detector_ctd: CtdTextDetectorService
    text_detector_paddle: PaddleTextDetectorService
    text_detector_surya: SuryaTextDetectorService
    lama_inpaint: LamaInpaintService
    lama_mpe_inpaint: LamaMpeInpaintService
    aot_inpaint: AotInpaintService
    sdxl_inpaint: SdxlInpaintService
    flux_fill_inpaint: FluxFillInpaintService
    watermark: WatermarkRemovalService
    reline: RelineService
    machine_translation: MachineTranslationService
    ai_device: AiDeviceService
    browser: BrowserService
    health_snapshot: dict[str, Any] = field(default_factory=dict)
    health_snapshot_lock: threading.Lock = field(default_factory=threading.Lock, repr=False)


def _safe_service_health(service: Any) -> dict[str, Any]:
    """Call `service.health()` but never let one failing service kill the snapshot.

    `_build_health_snapshot` aggregates every service's `.health()`; some of those
    (e.g. `surya.health()` imports torch unconditionally) can raise. If any single
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
        },
        # Watermark removal is a domain of its own, not an inpaint engine: it
        # predicts a mask instead of consuming one, so it gets a top-level key.
        "watermark": _safe_service_health(state.watermark),
        "image_processing": {
            "reline": _safe_service_health(state.reline),
        },
        "machine_translation": _safe_service_health(state.machine_translation),
        "model_manager": _safe_service_health(state.model_manager),
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
    model_manager = LoadedModelManager()
    onnx_runtime_factory = RuntimeFactory(model_manager)
    ai_device_service = AiDeviceService(model_manager)
    # Shared so the SDXL 4-channel prefill reuses the same LaMa model cache.
    lama_inpaint_service = LamaInpaintService(model_manager)
    state = AppState(
        app_version=app_version,
        model_manager=model_manager,
        easy_ocr=EasyOcrService(model_manager),
        manga_ocr=MangaOcrService(model_manager, ai_device_service),
        paddle_ocr=PaddleOcrService(onnx_runtime_factory),
        paddle_vl_ocr=PaddleVlOcrService(model_manager),
        surya_ocr=SuryaOcrService(model_manager),
        text_detector_ctd=CtdTextDetectorService(model_manager),
        text_detector_paddle=PaddleTextDetectorService(onnx_runtime_factory),
        text_detector_surya=SuryaTextDetectorService(model_manager),
        lama_inpaint=lama_inpaint_service,
        lama_mpe_inpaint=LamaMpeInpaintService(model_manager),
        aot_inpaint=AotInpaintService(model_manager),
        sdxl_inpaint=SdxlInpaintService(model_manager, lama_inpaint_service),
        flux_fill_inpaint=FluxFillInpaintService(model_manager),
        watermark=WatermarkRemovalService(model_manager),
        reline=RelineService(),
        machine_translation=MachineTranslationService(),
        ai_device=ai_device_service,
        browser=BrowserService(),
    )
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
