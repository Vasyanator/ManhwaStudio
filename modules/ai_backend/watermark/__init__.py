"""
Package: modules/ai_backend/watermark

Visible-watermark detection and removal service domain of the Python AI backend
(`watermark.detect` / `.remove` / `.status` / `.unload`). `service.py` owns
`WatermarkRemovalService`; `code_fetch.py` owns the runtime download and import
of the three upstream networks (SLBR, WDNet, SplitNet), whose code is never
vendored because no upstream repository carries a LICENSE.

This file deliberately exposes NOTHING — no re-exports and no imports of its own
submodules, matching the contract every other service domain follows
(`modules/ai_backend/MODULE_README.md`): importing one domain must never drag
another's dependency stack (here: torch) into the process, and the torch-free
`ipc/` layer must stay torch-free. Import submodules directly, e.g.
`from .watermark.service import WatermarkRemovalService`.

See `MODULE_README.md` in this directory for the package contracts, in
particular the runtime code-fetch rules (pinned commits, mandatory SHA-256,
never edit a downloaded file) and the resolution strategy that separates the
cheap whole-image `detect` pass from the experimental tiled `remove` pass.
"""
