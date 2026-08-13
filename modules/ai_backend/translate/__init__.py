"""
Package: modules/ai_backend/translate

Machine-translation adapters for the Python AI backend. `machine_translation.py`
wraps the third-party `deep_translator` providers behind
`MachineTranslationService`, backing the `translate.deep` IPC method.

Intentionally empty: no re-exports and no submodule imports, so importing this
package never pulls in `deep_translator`. Import
`modules.ai_backend.translate.machine_translation` explicitly when the service is
needed.
"""

from __future__ import annotations
