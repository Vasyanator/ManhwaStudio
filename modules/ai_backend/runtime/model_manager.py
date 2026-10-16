"""
FILE OVERVIEW: modules/ai_backend/runtime/model_manager.py
The small-model pool: the policy of every whole-model PyTorch / ONNX service
(OCR, detection, LaMa/AOT/SDXL inpaint, watermark, ONNX sessions) over the
shared residency arbiter (`residency/arbiter.py`), as a peer of the large-model
pool.

Main responsibilities:
- the service-facing lease API (`begin_model_use`, `ModelUsageLease`,
  `mark_unloaded`) over `ResidencyArbiter` with `pool=SMALL`;
- the small pool's reclaim policy (`_SmallPoolPolicy`): drop whole idle models,
  least recently used first (offer class B);
- the secondary count cap `General.ai_max_loaded_models` as slots: an admission
  needs one slot, a lowered cap evicts idle models live;
- `health()`: the four counters of the small pool (IPC contract).

Key structures:
- `SmallModelManager`
- `ModelUsageLease` (defined in `residency/arbiter.py`, returned from here)

Notes:
- The manager does not load models; services keep ownership of their runtime
  objects and report load/unload transitions.
- Eviction callbacks run outside every manager lock, so a service lock can be
  taken inside them.
- Lease protocol (callers): `begin_model_use` BEFORE the service lock, then,
  under the service lock, the load and `mark_loaded()` UNCONDITIONALLY once the
  service holds the model (`needs_load` only says whether the manager expected
  a load; a concurrent model switch can force a reload under a lease that found
  the key resident, and that reload must be registered too), `mark_load_failed()`
  only from the load scope, `release()` in a `finally`.
- `owner=` / `hint=` of `begin_model_use` are optional: without them a lease
  reserves no bytes (only the slot) and its owner is the key itself, which is
  exactly the pre-arbiter behaviour.
"""

from __future__ import annotations

from collections.abc import Sequence

from .residency.arbiter import ModelUsageLease, ResidencyArbiter, UnloadCallback
from .residency.holdings import (
    Action,
    FootprintHint,
    Freed,
    HoldingsView,
    HoldingView,
    Need,
    Offer,
    OfferClass,
    Pool,
    Requester,
)

DEFAULT_MAX_LOADED_MODELS = 3
MIN_MAX_LOADED_MODELS = 1
MAX_MAX_LOADED_MODELS = 10


def clamp_max_loaded_models(value: object) -> int:
    """`value` as an int clamped to [1, 10]; anything unparsable is the default 3."""
    try:
        normalized = int(value)  # type: ignore[call-overload]
    except (TypeError, ValueError, OverflowError):
        normalized = DEFAULT_MAX_LOADED_MODELS
    return max(MIN_MAX_LOADED_MODELS, min(MAX_MAX_LOADED_MODELS, normalized))


class _SmallPoolPolicy:
    """`PoolPolicy` of the small pool: whole-model drops of idle models, LRU; the count cap as slots.

    `slot_capacity()` is read under the arbiter lock, so the cap is a plain
    attribute written by `SmallModelManager.set_max_loaded_models`.
    """

    pool = Pool.SMALL

    def __init__(self, max_loaded_models: int) -> None:
        self.max_loaded_models = max_loaded_models

    def slot_capacity(self) -> int | None:
        """The count cap `General.ai_max_loaded_models`."""
        return self.max_loaded_models

    def offers(self, view: HoldingsView, need: Need, requester: Requester | None) -> Sequence[Offer]:
        """One `DROP` per idle small model that has an unload callback (pure over `view`).

        `reload_cost_s` stays `0.0`: inside class B small models are ordered by
        LRU only, as before the arbiter.
        """
        offers: list[Offer] = []
        for holding in view.of_pool(Pool.SMALL):
            if not holding.idle or holding.reclaim is None:
                continue
            offers.append(_drop_offer(holding))
        return offers

    def execute(self, offer: Offer) -> Freed:
        """Run the model's unload callback (outside every manager lock); `ok` is its result."""
        callback = offer.handle
        if not callable(callback):
            return Freed(device_bytes=0, host_bytes=0, ok=False)
        unloaded = bool(callback())
        if not unloaded:
            return Freed(device_bytes=0, host_bytes=0, ok=False)
        return Freed(device_bytes=offer.frees_device, host_bytes=offer.frees_host, ok=True)


def _drop_offer(holding: HoldingView) -> Offer:
    """The class-B whole-model drop of one idle small holding."""
    footprint = holding.footprint
    return Offer(
        pool=Pool.SMALL,
        key=holding.key,
        part=None,
        action=Action.DROP,
        device=footprint.device,
        frees_device=footprint.device_bytes if footprint.device is not None else 0,
        frees_host=footprint.host_bytes,
        reload_cost_s=0.0,
        cls=OfferClass.B,
        last_used_at=holding.last_used_at,
        frees_slots=1,
        handle=holding.reclaim,
    )


class SmallModelManager:
    """The small-model pool over a `ResidencyArbiter` (its own one unless `arbiter` is shared).

    The backend constructs ONE arbiter and passes it to this pool and to the
    large-model pool; a standalone instance (tests, tools) gets a private
    arbiter with the real memory probes.
    """

    def __init__(self, max_loaded_models: int = DEFAULT_MAX_LOADED_MODELS, *, arbiter: ResidencyArbiter | None = None) -> None:
        self._arbiter = arbiter if arbiter is not None else ResidencyArbiter()
        self._policy = _SmallPoolPolicy(clamp_max_loaded_models(max_loaded_models))
        self._arbiter.register_pool(self._policy)

    @property
    def arbiter(self) -> ResidencyArbiter:
        """The arbiter this pool's leases live in."""
        return self._arbiter

    def begin_model_use(
        self,
        model_key: str,
        unload_callback: UnloadCallback | None = None,
        *,
        owner: str | None = None,
        hint: FootprintHint | None = None,
    ) -> ModelUsageLease:
        """Take a lease on `model_key`, waiting while it is being loaded or evicted elsewhere.

        Returns a lease with `needs_load=True` when the caller must load the
        model (the entry is then flagged `loading`), else `needs_load=False`.
        A load first reclaims: idle small models (LRU) down to the count cap,
        and — when `hint` gives its size — idle models of either pool until its
        bytes fit. `owner` names the service (the owner rule of the arbiter:
        an idle model of a service with another lease in use is never evicted);
        it defaults to the key. `hint` defaults to `None`: no byte reservation.
        Raises `ValueError` for an empty key and `ModelMemoryShortError` (a
        `RuntimeError`) when the cap or the memory is exhausted with nothing
        evictable. Must be called BEFORE the service lock (it may wait on an
        eviction callback that takes that lock).
        """
        return self._arbiter.begin_use(Pool.SMALL, model_key, owner=owner, hint=hint, on_reclaim=unload_callback)

    def finish_load(self, model_key: str, unload_callback: UnloadCallback | None = None) -> None:
        """Mark `model_key` resident and not loading; idempotent (re-registering a resident key only refreshes it)."""
        self._arbiter.finish_load(Pool.SMALL, model_key, unload_callback)

    def abort_load(self, model_key: str) -> None:
        """Abort a reserved load of `model_key` (clears `loading`; the entry goes when unused)."""
        self._arbiter.abort_load(Pool.SMALL, model_key)

    def release(self, model_key: str) -> None:
        """Drop one in-use count of `model_key` (a lease's `release` is the normal path)."""
        self._arbiter.release(Pool.SMALL, model_key)

    def mark_unloaded(self, model_key: str) -> None:
        """The service dropped `model_key`; the entry survives (not resident) while a lease uses it."""
        self._arbiter.mark_unloaded(Pool.SMALL, model_key)

    def get_max_loaded_models(self) -> int:
        """The current count cap."""
        return self._policy.max_loaded_models

    def set_max_loaded_models(self, value: object) -> int:
        """Set the count cap (clamped) and evict idle models down to it now; returns the cap set."""
        normalized = clamp_max_loaded_models(value)
        self._policy.max_loaded_models = normalized
        self._arbiter.trim_slots(Pool.SMALL)
        return normalized

    def health(self) -> dict[str, int]:
        """Counters for the health snapshot (small pool only; the payload keys are an IPC contract)."""
        counters = self._arbiter.pool_counters(Pool.SMALL)
        return {
            "max_loaded_models": self._policy.max_loaded_models,
            "resident_model_count": counters["resident"],
            "active_model_count": counters["in_use"],
            "loading_model_count": counters["loading"],
        }

    def holdings(self) -> dict[str, HoldingView]:
        """Snapshot `{key: HoldingView}` of the small pool."""
        return self._arbiter.holdings(Pool.SMALL)
