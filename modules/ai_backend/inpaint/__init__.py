"""
Package: modules/ai_backend/inpaint

Inpainting service adapters for the Python AI backend: LaMa V2, LaMa MPE, AOT,
SDXL and FLUX.1-Fill-dev. Each module owns one backend and is wired into
`AppState` by `server.py`; the IPC surface lives in `ipc/handlers/`.

This file deliberately exposes NOTHING - no re-exports and no imports of its own
submodules. The backends have wildly different dependency footprints (`sdxl` and
`flux_fill` pull in diffusers/transformers, `aot` and `lama_mpe` pull in torch),
so a re-export here would make importing the cheapest inpainter drag in the
heaviest one. Import submodules directly, e.g.
`from .inpaint.lama import LamaInpaintService`.

See `MODULE_README.md` in this directory for the package contracts, in
particular the path-based dynamic load chain
`lama.py` -> `lama_v2_runtime_inpainter.py` -> `lama_runtime_bundle/`.
"""
