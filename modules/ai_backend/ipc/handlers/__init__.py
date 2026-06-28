"""
Package: modules/ai_backend/ipc/handlers

This package contains one module per feature group.  Each module
self-registers its handlers into the shared ``METHOD_HANDLERS`` dict via
``registry.register()`` (or its decorator alias) at import time.

Importing this package (``from . import handlers``, or equivalently
``import modules.ai_backend.ipc.handlers``) is all that is needed to wire
every group into the registry.  The registry module does exactly that:

    # in registry.py — the ONLY shared touch-point for future agents:
    from . import handlers  # noqa: F401 — side-effect: registers all methods

To add a brand-new group, a parallel agent:
  1. Creates ``handlers/<group>.py`` with handler functions and
     ``register(METHOD_X, _handle_x)`` calls at module level.
  2. Adds ONE import line here, in the block below, e.g.:
         from . import mygroup  # noqa: F401

That single import line in THIS file is the only shared touch-point for
parallel agents — they never edit registry.py or each other's group files.
"""

# ---------------------------------------------------------------------------
# Handler group imports — each import triggers self-registration.
# To add a new group, append exactly one line here (and nowhere else).
# ---------------------------------------------------------------------------
from . import health       # noqa: F401  — health
from . import ocr          # noqa: F401  — ocr.manga (+ future ocr.* methods)
from . import textdetector # noqa: F401  — textdetector.ctd / .paddle / .surya
from . import inpaint      # noqa: F401  — inpaint.lama_v2 / .lama_mpe / .aot (+ unloads)
from . import sdxl         # noqa: F401  — inpaint.sdxl (+ unload)
from . import reline       # noqa: F401  — reline.models / reline.process
from . import device       # noqa: F401  — device.get / .set / .cuda_diagnostics
from . import translate    # noqa: F401  — translate.deep
from . import browser      # noqa: F401  — browser.command (Selenium / CloakBrowser)
from . import flux_fill     # noqa: F401  — inpaint.flux_fill (+ unload, + status)
