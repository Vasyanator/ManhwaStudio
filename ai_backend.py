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
    run_server(
        socket_path=socket_path,
        warmup_mangaocr=args.warmup_mangaocr,
        app_version=VERSION,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
