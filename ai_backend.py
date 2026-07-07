#!/usr/bin/env python3
"""
File: ai_backend.py

Purpose:
Entrypoint for the local Python AI backend process used by the Rust application.

Main responsibilities:
- bootstrap the project-local Python modules path;
- configure backend logging and Torch availability mode;
- apply ROCm/MIOpen immediate-mode runtime settings before any inference;
- resolve the AF_UNIX listening socket path (default standard path or `--socket`);
- pass the Python-side application version into the backend health contract.

Notes:
The backend listens on a single AF_UNIX domain socket (not a TCP host/port) and
speaks the framed, multiplexed IPC protocol (see
`modules/ai_backend/ipc/PROTOCOL.md`). When launched manually with no arguments
it uses the standard per-platform socket path; ManhwaStudio passes `--socket
PATH` explicitly.
"""

from __future__ import annotations

import argparse
import logging
import os
import sys
import tempfile
from pathlib import Path


def _bootstrap_python_modules() -> None:
    root = Path(__file__).resolve().parent
    modules_dir = root / "modules"
    modules_path = str(modules_dir)
    if modules_path not in sys.path:
        sys.path.insert(0, modules_path)


def default_socket_path() -> Path:
    """Return the standard AF_UNIX socket path the backend listens on by default.

    This path is the cross-process contract shared with the Rust side and must
    match it byte-for-byte. On Windows it lives under the system temp directory
    (`tempfile.gettempdir()`); on posix it is a fixed `/tmp` path.
    """
    name = "manhwastudio_backend_socket"
    if os.name == "nt":
        return Path(tempfile.gettempdir()) / name
    return Path("/tmp") / name


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Microservice for AI tasks used by Rust frontend.",
    )
    parser.add_argument(
        "--socket",
        type=str,
        default=None,
        help=(
            "AF_UNIX socket path to listen on. Defaults to the standard "
            "per-platform path when omitted."
        ),
    )
    parser.add_argument(
        "--transport",
        type=str,
        choices=["unix", "ws"],
        default=None,
        help=(
            "IPC transport: 'unix' (AF_UNIX socket, default) or 'ws' "
            "(token-authenticated WebSocket fallback). Falls back to "
            "MS_BACKEND_TRANSPORT, then 'unix'."
        ),
    )
    parser.add_argument(
        "--ws-host",
        type=str,
        default=None,
        help=(
            "Host to bind for the 'ws' transport. Falls back to WS_HOST, then "
            "'127.0.0.1'. Ignored for the 'unix' transport."
        ),
    )
    parser.add_argument(
        "--ws-port",
        type=int,
        default=None,
        help=(
            "TCP port to bind for the 'ws' transport (0 = ephemeral, the bound "
            "port is printed as MS_BACKEND_WS_PORT=<port>). Falls back to "
            "WS_PORT, then 0. Ignored for the 'unix' transport."
        ),
    )
    parser.add_argument(
        "--ws-token",
        type=str,
        default=None,
        help=(
            "Shared secret required in the WS handshake '?token=' query param "
            "for the 'ws' transport. Falls back to WS_TOKEN. Required when "
            "transport is 'ws'."
        ),
    )
    parser.add_argument(
        "--warmup-mangaocr",
        action="store_true",
        help="Warm up MangaOCR model in background right after startup.",
    )
    parser.add_argument(
        "--simulate-disabled-torch",
        action="store_true",
        help="Debug mode: force backend to behave as if PyTorch is not installed.",
    )
    return parser


def _env_ws_port() -> int:
    """Return the WS port from the WS_PORT env var, or 0 (ephemeral) if unset/invalid.

    An unset or non-integer `WS_PORT` yields 0 so the WS server binds an
    ephemeral port; a clear log line records a malformed value rather than
    crashing startup.
    """
    raw = os.environ.get("WS_PORT")
    if raw is None:
        return 0
    try:
        return int(raw)
    except ValueError:
        logging.getLogger(__name__).warning(
            "Ignoring invalid WS_PORT=%r; falling back to an ephemeral port (0).",
            raw,
        )
        return 0


def _configure_logging() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    )


def main() -> int:
    _bootstrap_python_modules()
    _configure_logging()
    from config import VERSION
    from ai_backend.torch_support import configure_torch_support
    from ai_backend.rocm_runtime import configure_rocm_runtime
    from ai_backend.server import run_server

    args = _build_parser().parse_args()
    configure_torch_support(simulate_disabled=args.simulate_disabled_torch)
    # Apply MIOpen immediate-mode + persistent-cache settings before any Torch
    # inference so ROCm builds skip repeated per-shape kernel auto-tuning. No-op
    # on CUDA/CPU/MPS/absent-Torch installs.
    configure_rocm_runtime()
    # An explicit --socket overrides the standard path; otherwise fall back to
    # the per-platform default shared with the Rust side.
    socket_path = Path(args.socket) if args.socket else default_socket_path()
    # Resolve transport options with env fallbacks so the Rust supervisor can
    # pass them via either CLI flags or the environment. CLI flag wins; then the
    # env var; then the built-in default.
    transport = args.transport or os.environ.get("MS_BACKEND_TRANSPORT") or "unix"
    ws_host = args.ws_host or os.environ.get("WS_HOST") or "127.0.0.1"
    ws_port = args.ws_port if args.ws_port is not None else _env_ws_port()
    ws_token = args.ws_token or os.environ.get("WS_TOKEN")
    run_server(
        socket_path=socket_path,
        warmup_mangaocr=args.warmup_mangaocr,
        app_version=VERSION,
        transport=transport,
        ws_host=ws_host,
        ws_port=ws_port,
        ws_token=ws_token,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
