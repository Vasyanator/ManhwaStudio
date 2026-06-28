"""
File: modules/ai_backend/server.py

Purpose:
Runtime that builds all local AI services and serves them over the framed IPC
protocol used by the Rust application.

Main responsibilities:
- construct the shared `AppState` (OCR / text detector / inpaint / translation /
  device services) consumed by the IPC handlers in `ipc/handlers/`;
- maintain a non-blocking health snapshot and publish it as a `health` event on
  the IPC event bus;
- expose the backend version metadata for Rust-side compatibility checks.

Transport:
The backend listens on a single AF_UNIX domain socket (not TCP) and speaks the
framed, multiplexed IPC protocol (see `ipc/frame_server.py` and `ipc/PROTOCOL.md`).
`run_server(socket_path=...)` runs the frame server on that base path in the
foreground, enforcing a single live instance via stale-socket detection and
unlinking the socket file on shutdown. AF_UNIX works on Linux and Windows 10 1803+.
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

from .device_service import AiDeviceService
from .ctd_text_detector_service import CtdTextDetectorService
from .paddle_text_detector_service import PaddleTextDetectorService
from .easy_ocr_service import EasyOcrService
from .aot_inpaint_service import AotInpaintService
from .lama_mpe_inpaint_service import LamaMpeInpaintService
from .lama_inpaint_service import LamaInpaintService
from .manga_ocr_service import MangaOcrService
from .machine_translation_service import MachineTranslationService
from .model_manager import LoadedModelManager
from .paddle_ocr_service import PaddleOcrService
from .paddle_vl_ocr_service import PaddleVlOcrService
from .paddle_onnx_runtime import RuntimeFactory
from .reline_service import RelineService
from .sdxl_inpaint_service import SdxlInpaintService
from .flux_fill_inpaint_service import FluxFillInpaintService
from .surya_ocr_service import SuryaOcrService
from .surya_text_detector_service import SuryaTextDetectorService
from .torch_support import is_torch_available
from .browser.service import BrowserService
from .ipc.protocol import TOPIC_HEALTH

HEALTH_SNAPSHOT_REFRESH_SECS = 1.0

# ============================================================================
# AI BACKEND SERVER
# ----------------------------------------------------------------------------
# Что в файле:
# - AppState: shared сервисы OCR/MT/Inpaint/textdetector/device.
# - `_build_health_snapshot`/`_health_snapshot_worker`: фоновой health-snapshot,
#   который также публикуется как `health` event на шину IPC, поэтому клиентам не
#   нужно опрашивать health.
# - `run_server`: строит сервисы и запускает framed IPC frame-server на базовом
#   AF_UNIX-сокете (единственный транспорт). Маршрутизация запросов живёт в
#   `ipc/handlers/`, а не здесь.
# ============================================================================

@dataclass
class AppState:
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
) -> None:
    """Build all AI services and serve the framed IPC protocol over an AF_UNIX socket.

    `socket_path` is the base AF_UNIX path to bind (the single, sole IPC
    transport). A live backend already on that path raises
    `FrameBackendInstanceError`; a stale socket file is replaced. The frame
    server enforces single-instance ownership and `chmod 0o600` on the socket,
    runs in the foreground until interrupted, then unlinks the socket file.

    Raises RuntimeError on a Python build without AF_UNIX support.
    """
    # AF_UNIX is required on every supported platform; fail loudly on a Python
    # build that lacks it instead of an obscure AttributeError at bind time.
    if not hasattr(socket, "AF_UNIX"):
        raise RuntimeError(
            "This Windows build of Python lacks AF_UNIX support; "
            "Windows 10 1803+ with a modern CPython is required."
        )
    socket_path_str = os.fspath(socket_path)
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
    # The single AF_UNIX socket runs the framed, multiplexed protocol. The frame
    # server owns the event bus; the health worker publishes snapshots to it so
    # clients receive health pushes instead of polling. Request routing lives in
    # `ipc/handlers/`, which reach the AppState services directly via the handler
    # context.
    from .ipc.events import EventBus
    from .ipc.frame_server import run_frame_server

    event_bus = EventBus()

    health_thread = threading.Thread(
        target=_health_snapshot_worker,
        args=(state, stop_event, event_bus),
        daemon=True,
    )
    health_thread.start()
    if warmup_mangaocr:
        threading.Thread(target=_warmup_safe, args=(state,), daemon=True).start()

    print(f"[AI Backend] Running framed IPC on unix socket {socket_path_str}")
    try:
        run_frame_server(
            state,
            socket_path_str,
            stop_event,
            backend_version=state.app_version,
            get_health_snapshot=lambda: _get_health_snapshot(state),
            events=event_bus,
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
