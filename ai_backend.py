#!/usr/bin/env python3
"""
File: ai_backend.py

Purpose:
Entrypoint for the local Python AI backend process used by the Rust application.

Main responsibilities:
- bootstrap the project-local Python modules path;
- configure backend logging and Torch availability mode;
- pass the Python-side application version into the HTTP backend health contract.
"""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path


def _bootstrap_python_modules() -> None:
    root = Path(__file__).resolve().parent
    modules_dir = root / "modules" 
    modules_path = str(modules_dir)
    if modules_path not in sys.path:
        sys.path.insert(0, modules_path)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Microservice for AI tasks used by Rust frontend.",
    )
    parser.add_argument("--host", default="127.0.0.1", help="Bind host.")
    parser.add_argument("--port", type=int, default=8765, help="Bind port.")
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
    from ai_backend.server import run_server

    args = _build_parser().parse_args()
    configure_torch_support(simulate_disabled=args.simulate_disabled_torch)
    run_server(
        host=args.host,
        port=args.port,
        warmup_mangaocr=args.warmup_mangaocr,
        app_version=VERSION,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
