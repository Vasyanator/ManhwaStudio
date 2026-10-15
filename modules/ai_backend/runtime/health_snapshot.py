"""
File: modules/ai_backend/runtime/health_snapshot.py

Purpose:
The one owner of how a backend service publishes its `health()` fields without
making a health query wait for that service's work lock.

Every model service guards its runtime objects with an instance lock that a
model load or an inference holds for seconds to minutes. `server.py`'s health
worker calls each service's `health()` in turn to build the snapshot it pushes
to Rust, so one `health()` that took that lock stalled the whole pushed health
snapshot (and therefore every AI-button gate in the app) for the length of the
load. A service therefore keeps its health fields in a `HealthSnapshot`: it
republishes them under the snapshot's own short lock whenever the guarded
state changes, and `health()` only reads the snapshot.

Key structures:
- HealthSnapshot

Notes:
Stdlib only, like `paths.py`: imported by every service domain and must not pull
anything heavy in. Lock order is always "service lock, then snapshot lock" (a
service publishes while it holds its own lock); the snapshot lock is never held
while taking another lock, so no cycle exists.
"""

from __future__ import annotations

import threading
from typing import Any


class HealthSnapshot:
    """Copy-on-write health fields of one service, readable without its work lock.

    `publish` merges new values under a short private lock and replaces the
    stored dict as a whole, so a reader never sees half of one publication.
    Callers pass values they never mutate afterwards (fresh lists / tuples /
    strings); `read` returns a shallow copy of the dict. Thread-safe; neither
    method blocks for longer than one dict copy.
    """

    def __init__(self, **initial: Any) -> None:
        self._lock = threading.Lock()
        self._fields: dict[str, Any] = dict(initial)

    def publish(self, **fields: Any) -> None:
        """Merge `fields` into the published snapshot (other keys keep their values)."""
        with self._lock:
            merged = dict(self._fields)
            merged.update(fields)
            self._fields = merged

    def read(self) -> dict[str, Any]:
        """Return a shallow copy of the currently published fields."""
        with self._lock:
            return dict(self._fields)
