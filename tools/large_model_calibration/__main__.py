"""
File: tools/large_model_calibration/__main__.py

Purpose:
Entry point of the large-model calibration tool when the DIRECTORY is run:
`venv/bin/python tools/large_model_calibration <subcommand> [options]` from the
repository root.

Main responsibilities:
- re-exec into the project interpreter `<repo>/venv/bin/python` when it
  exists and another interpreter runs the tool (a checkout without `venv/`,
  e.g. a measurement host with its own environment, runs as invoked);
- set `PROTOCOL_BUFFERS_PYTHON_IMPLEMENTATION=python` before any import that
  may pull protobuf (transformers / sentencepiece clash under upb);
- put the repository root (for `modules.ai_backend...`) and `tools/` (for this
  package, `large_model_calibration.*`) on `sys.path`, then hand over to
  `cli.main`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

os.environ.setdefault("PROTOCOL_BUFFERS_PYTHON_IMPLEMENTATION", "python")

#: This package's directory, `tools/`, and the repository root.
_PACKAGE_DIR = Path(__file__).resolve().parent
_TOOLS_DIR = _PACKAGE_DIR.parent
_REPO_ROOT = _TOOLS_DIR.parent

#: Marker that stops a second re-exec (the venv interpreter may be a symlink chain).
_REEXEC_ENV = "MS_LARGE_MODEL_CALIBRATION_REEXEC"


def _ensure_project_venv() -> None:
    """Re-exec this tool with `<repo>/venv/bin/python` when another interpreter runs it.

    Does nothing when the venv is missing (the imports below then fail with
    their own clear error, or another environment provides them) or when this
    already is that interpreter.
    """
    venv_python = _REPO_ROOT / "venv" / "bin" / "python"
    if not venv_python.is_file():
        return
    if Path(sys.executable).resolve() == venv_python.resolve() or os.environ.get(_REEXEC_ENV) == "1":
        return
    os.environ[_REEXEC_ENV] = "1"
    os.execv(str(venv_python), [str(venv_python), str(_PACKAGE_DIR), *sys.argv[1:]])


def _setup_import_path() -> None:
    """Drop the package directory itself from `sys.path` (its module names are
    generic) and add the repository root first and `tools/` last (so nothing in
    `tools/` can shadow an installed module)."""
    sys.path[:] = [entry for entry in sys.path if not entry or Path(entry).resolve() != _PACKAGE_DIR]
    sys.path.insert(0, str(_REPO_ROOT))
    if str(_TOOLS_DIR) not in sys.path:
        sys.path.append(str(_TOOLS_DIR))


if __name__ == "__main__":
    _ensure_project_venv()
    _setup_import_path()
    from large_model_calibration.cli import main

    sys.exit(main(sys.argv[1:], repo_root=_REPO_ROOT))
