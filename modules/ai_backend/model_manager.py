"""
FILE OVERVIEW: modules/ai_backend/model_manager.py
Shared loaded-model manager for Python AI backend runtimes.

Main responsibilities:
- Track loaded model/session entries across PyTorch and ONNX services.
- Enforce a configurable upper bound for simultaneously resident models.
- Evict least recently used idle models before loading a new one.
- Prevent unloading models that are currently used by active requests.

Key structures:
- `LoadedModelManager`
- `ModelUsageLease`

Notes:
- The manager does not perform model loading itself; services keep ownership of
  their concrete runtime objects and report load/unload lifecycle transitions.
- Eviction callbacks are always executed outside the manager lock to avoid
  deadlocks with service-local locks.
"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass
from typing import Callable

UnloadCallback = Callable[[], bool]

DEFAULT_MAX_LOADED_MODELS = 3
MIN_MAX_LOADED_MODELS = 1
MAX_MAX_LOADED_MODELS = 10


def clamp_max_loaded_models(value: object) -> int:
    try:
        normalized = int(value)
    except Exception:
        normalized = DEFAULT_MAX_LOADED_MODELS
    return max(MIN_MAX_LOADED_MODELS, min(MAX_MAX_LOADED_MODELS, normalized))


@dataclass
class _ModelEntry:
    resident: bool = False
    loading: bool = False
    evicting: bool = False
    in_use_count: int = 0
    last_used_at: float = 0.0
    unload_callback: UnloadCallback | None = None


class ModelUsageLease:
    def __init__(self, manager: "LoadedModelManager", model_key: str, needs_load: bool) -> None:
        self._manager = manager
        self._model_key = model_key
        self.needs_load = needs_load
        self._load_finished = not needs_load
        self._released = False

    def mark_loaded(self, unload_callback: UnloadCallback | None = None) -> None:
        if self._load_finished:
            return
        self._manager.finish_load(self._model_key, unload_callback)
        self._load_finished = True

    def mark_load_failed(self) -> None:
        if self._load_finished:
            return
        self._manager.abort_load(self._model_key)
        self._load_finished = True

    def release(self) -> None:
        if self._released:
            return
        self._manager.release(self._model_key)
        self._released = True


class LoadedModelManager:
    def __init__(self, max_loaded_models: int = DEFAULT_MAX_LOADED_MODELS) -> None:
        self._condition = threading.Condition()
        self._entries: dict[str, _ModelEntry] = {}
        self._max_loaded_models = clamp_max_loaded_models(max_loaded_models)

    def begin_model_use(
        self,
        model_key: str,
        unload_callback: UnloadCallback | None = None,
    ) -> ModelUsageLease:
        normalized_key = str(model_key).strip()
        if not normalized_key:
            raise ValueError("model_key must be non-empty")

        while True:
            with self._condition:
                entry = self._entries.setdefault(normalized_key, _ModelEntry())
                if unload_callback is not None:
                    entry.unload_callback = unload_callback
                if entry.evicting:
                    self._condition.wait()
                    continue
                if entry.resident:
                    entry.in_use_count += 1
                    entry.last_used_at = time.monotonic()
                    return ModelUsageLease(self, normalized_key, needs_load=False)
                if entry.loading:
                    self._condition.wait()
                    continue
                entry.loading = True
                entry.in_use_count += 1
                entry.last_used_at = time.monotonic()
                break

        try:
            self._ensure_capacity_for_new_load(normalized_key)
        except Exception:
            self.abort_load(normalized_key)
            self.release(normalized_key)
            raise

        return ModelUsageLease(self, normalized_key, needs_load=True)

    def finish_load(
        self,
        model_key: str,
        unload_callback: UnloadCallback | None = None,
    ) -> None:
        with self._condition:
            entry = self._entries.setdefault(model_key, _ModelEntry())
            entry.loading = False
            entry.resident = True
            entry.evicting = False
            entry.last_used_at = time.monotonic()
            if unload_callback is not None:
                entry.unload_callback = unload_callback
            self._condition.notify_all()

    def abort_load(self, model_key: str) -> None:
        with self._condition:
            entry = self._entries.get(model_key)
            if entry is None:
                return
            entry.loading = False
            entry.evicting = False
            self._cleanup_entry_if_unused_locked(model_key, entry)
            self._condition.notify_all()

    def release(self, model_key: str) -> None:
        with self._condition:
            entry = self._entries.get(model_key)
            if entry is None:
                return
            if entry.in_use_count > 0:
                entry.in_use_count -= 1
            entry.last_used_at = time.monotonic()
            self._cleanup_entry_if_unused_locked(model_key, entry)
            self._condition.notify_all()

    def mark_unloaded(self, model_key: str) -> None:
        with self._condition:
            entry = self._entries.get(model_key)
            if entry is None:
                return
            entry.resident = False
            entry.loading = False
            entry.evicting = False
            entry.last_used_at = time.monotonic()
            self._cleanup_entry_if_unused_locked(model_key, entry)
            self._condition.notify_all()

    def get_max_loaded_models(self) -> int:
        with self._condition:
            return self._max_loaded_models

    def set_max_loaded_models(self, value: object) -> int:
        normalized = clamp_max_loaded_models(value)
        with self._condition:
            self._max_loaded_models = normalized
        self._evict_idle_until_within_limit()
        return normalized

    def health(self) -> dict[str, int]:
        with self._condition:
            resident = 0
            in_use = 0
            loading = 0
            for entry in self._entries.values():
                if entry.resident:
                    resident += 1
                if entry.loading:
                    loading += 1
                if entry.in_use_count > 0:
                    in_use += 1
            return {
                "max_loaded_models": self._max_loaded_models,
                "resident_model_count": resident,
                "active_model_count": in_use,
                "loading_model_count": loading,
            }

    def _ensure_capacity_for_new_load(self, exclude_key: str) -> None:
        skipped: set[str] = set()
        while True:
            with self._condition:
                if self._resident_count_locked() < self._max_loaded_models:
                    return
                victim = self._pick_evictable_key_locked(exclude_key, skipped)
                if victim is None:
                    raise RuntimeError(
                        "Не удалось загрузить новую модель: достигнут лимит загруженных моделей, "
                        "а свободных кандидатов для выгрузки нет."
                    )
                entry = self._entries[victim]
                entry.evicting = True
                unload_callback = entry.unload_callback

            unloaded = False
            try:
                if unload_callback is not None:
                    unloaded = bool(unload_callback())
            finally:
                with self._condition:
                    entry = self._entries.get(victim)
                    if entry is None:
                        continue
                    entry.evicting = False
                    if unloaded:
                        entry.resident = False
                        entry.last_used_at = time.monotonic()
                        self._cleanup_entry_if_unused_locked(victim, entry)
                    else:
                        skipped.add(victim)
                    self._condition.notify_all()

    def _evict_idle_until_within_limit(self) -> None:
        skipped: set[str] = set()
        while True:
            with self._condition:
                if self._resident_count_locked() <= self._max_loaded_models:
                    return
                victim = self._pick_evictable_key_locked(None, skipped)
                if victim is None:
                    return
                entry = self._entries[victim]
                entry.evicting = True
                unload_callback = entry.unload_callback

            unloaded = False
            try:
                if unload_callback is not None:
                    unloaded = bool(unload_callback())
            finally:
                with self._condition:
                    entry = self._entries.get(victim)
                    if entry is None:
                        continue
                    entry.evicting = False
                    if unloaded:
                        entry.resident = False
                        entry.last_used_at = time.monotonic()
                        self._cleanup_entry_if_unused_locked(victim, entry)
                    else:
                        skipped.add(victim)
                    self._condition.notify_all()

    def _resident_count_locked(self) -> int:
        return sum(1 for entry in self._entries.values() if entry.resident)

    def _pick_evictable_key_locked(
        self,
        exclude_key: str | None,
        skipped: set[str],
    ) -> str | None:
        candidates: list[tuple[float, str]] = []
        for key, entry in self._entries.items():
            if key == exclude_key or key in skipped:
                continue
            if not entry.resident or entry.loading or entry.evicting:
                continue
            if entry.in_use_count > 0:
                continue
            if entry.unload_callback is None:
                continue
            candidates.append((entry.last_used_at, key))
        if not candidates:
            return None
        candidates.sort(key=lambda item: item[0])
        return candidates[0][1]

    def _cleanup_entry_if_unused_locked(self, model_key: str, entry: _ModelEntry) -> None:
        if entry.resident or entry.loading or entry.evicting or entry.in_use_count > 0:
            return
        self._entries.pop(model_key, None)
