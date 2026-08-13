"""
File: modules/ai_backend/runtime/paths.py

Purpose:
Single owner of the "how deep is this file inside the installation" assumption
for the whole `modules/ai_backend` package.

Main responsibilities:
- resolve the program root (the directory that contains `config.py`,
  `modules/` and `ManhwaStudio_AI_Models/`) from this file's location.

Key functions:
- `program_root()`

Notes:
- Deliberately dependency-free: standard library only, no `torch`, no
  `config`, no sibling backend module. Every backend layer - including the
  torch-free `ipc/` one - must be able to import it without cost or side
  effects.
- The depth constant lives here and nowhere else. Moving this file between
  directory levels is the only edit that may change it.
"""

from __future__ import annotations

from pathlib import Path

# `modules/ai_backend/runtime/paths.py` -> parents[0] = runtime,
# parents[1] = ai_backend, parents[2] = modules, parents[3] = program root.
# This is the ONLY place in the package where that depth is encoded.
_PROGRAM_ROOT = Path(__file__).resolve().parents[3]


def program_root() -> Path:
    """Return the installation/repo root of the program.

    The returned directory is the one that contains `config.py`, `modules/` and
    (when models are installed next to the program) `ManhwaStudio_AI_Models/`.
    The value is derived once from this file's own location and is absolute and
    symlink-resolved.

    This function is the SINGLE owner of the package's directory-depth
    assumption. New code must never re-derive the root by counting
    `Path(__file__).resolve().parents[N]` itself - call this instead, so that
    moving a module between package directories cannot silently point it at the
    wrong root.
    """
    return _PROGRAM_ROOT
