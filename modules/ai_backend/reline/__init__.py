"""
Package: modules/ai_backend/reline

Reline super-resolution / image-processing adapter for the Python AI backend.
`service.py` owns model catalog resolution, checkpoint download/extraction under
`ManhwaStudio_AI_Models/side_models/Reline`, and pipeline construction for the
`reline.models` / `reline.process` IPC methods.

Intentionally empty: no re-exports and no submodule imports, so importing this
package never pulls in the Reline pipeline or its Torch dependency. Import
`modules.ai_backend.reline.service` explicitly when the service is needed.
"""

from __future__ import annotations
