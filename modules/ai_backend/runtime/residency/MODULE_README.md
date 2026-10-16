# Module: modules/ai_backend/runtime/residency

## Purpose
The single owner of "who holds which backend memory and what may be reclaimed for whom". Both
model pools — the small pool (`../model_manager.py`: OCR, detection, LaMa/AOT/SDXL, watermark,
ONNX sessions) and the large pool (`../large_models/`, FLUX.2 / FLUX.3 klein) — take their leases
from ONE `ResidencyArbiter` as equal peers: each registers a `PoolPolicy` that offers reclaims of
its own idle holdings, and neither pool imports the other. The usable bytes themselves come from
`../memory_budget.py`; this package only keeps the ledger and decides who to park, drop or refuse.

## Architecture
```
holdings.py   pure value types (Pool, Footprint, Need, Requester, Offer, Freed, Shortfall,
              HoldingsView with per-device DeviceAccount) + usable_device_headroom()
footprint.py  pure size rules: hint_from_files, unknown-size default, estimate,
              load-window and release-window measurement arithmetic
arbiter.py    ResidencyArbiter (one Condition), ModelUsageLease, Reservation, PoolPolicy and
              BudgetProbes protocols, MemoryBudgetProbes (real probes), ModelMemoryShortError
```
Stdlib only at import (torch only lazily inside the real probes; pinned by
`test_arbiter.ImportTests`). The backend builds one arbiter and hands it to both pools; a
standalone `SmallModelManager()` builds a private one with the real probes.

Reserve choreography (`ResidencyArbiter.reserve`): probe outside the lock -> under the lock
`headroom = probe - outstanding reservations`; if short, collect every policy's `offers`, filter
R1 and the owner rule, sort by R4, take the shortest prefix covering the deficit, mark it
`evicting` -> execute each offer outside every lock (a failed or raising one is skipped for this
call) -> re-probe, at most 3 rounds that freed something. A lease that reserved a load makes an
ADMISSION reservation (its slot, plus its estimated bytes when it carried a hint); `finish_load`
materializes it.

## Files and submodules
- `holdings.py`: every type a policy or a pool manager builds on; the slack-credit rule.
- `footprint.py`: small-model sizes (pre-load hint, default, measured, learned).
- `arbiter.py`: the lease state machine of both pools, reservations, reclaims, `view()`,
  `health()`, `trim_slots()`.
- `test_arbiter.py`: contract tests with fake probes and a fake large policy (reclaim order, R1,
  owner rule incl. the two-thread deadlock case and SDXL -> LaMa, waits, reservations, measurement,
  slack accounting, no torch import).
- `test_footprint.py`: the pure rules of `footprint.py` and `usable_device_headroom`.

## Contracts and invariants
- **One lock, a leaf.** The arbiter's `Condition` is the only lock it takes, and nothing is
  acquired under it: no probe, no callback, no policy `execute`, no logging. `Condition.wait`
  releases it. `PoolPolicy.offers` and `slot_capacity` are the only foreign code run under it and
  must be pure over the `HoldingsView` (no I/O, no probe, no lock, no callback).
- **R1:** an in-use, loading or evicting holding is never offered (the arbiter re-checks every
  offer against its own state, whatever a policy returns).
- **R4 order:** class A (park an idle large component device -> host, only while the host still
  fits the requester's host need plus the parks chosen so far) < B (drop idle small models) < C
  (drop an idle large component). Inside a class: lowest `reload_cost_s / freed bytes`, then LRU.
  Small models carry `reload_cost_s = 0`, so class B is pure LRU (the pre-arbiter order). An offer
  is taken only if it frees something still short (device bytes on the needed device, host bytes,
  slots of the requester's pool); when the ledger cannot cover a strict need's deficit nothing is evicted.
- **Owner rule (deadlock breaker).** An idle holding is offered only when its owner has no OTHER
  in-use lease; under IN_RUN the requester's own owner is excluded entirely. Owner defaults to the
  key itself, which is exactly the pre-arbiter behaviour. Argument: the arbiter lock is a leaf;
  a thread blocks under a service lock only during an IN_RUN reclaim or a long load/inference;
  IN_RUN requires a lease, so that owner is busy and none of its holdings is ever offered to
  another thread's reclaim, so no thread waits in `execute` for a lock held by a thread that waits
  for it. SDXL holds its own lease while it takes the LaMa lease, so SDXL's idle entries are never
  offered to that LaMa admission. Premise callers must keep: **no service holds its lock across a
  blocking manager call (`begin_model_use`, `reserve`) without holding a lease of its own owner.**
- **Waiting.** IN_RUN never waits (it gets a `Shortfall` and the large ladder degrades). ADMISSION
  waits on the Condition, at most `admission_wait_s` (30 s) in total, only while other threads
  have evictions in flight. `begin_use` still waits unboundedly while its own key is loading or
  being evicted (the pre-arbiter lease rule).
- **Headroom is always re-probed (risk K3);** the ledger only decides who to reclaim. Outstanding
  (unmaterialized) reservations are subtracted from the probe; a probe taken before a reservation
  materialized or bytes left the ledger is retaken.
- **Own torch slack is usable.** `mem_get_info().free` counts this process's cached-but-free
  torch blocks (`torch_reserved - torch_allocated`) as used. For a need whose allocations come
  from the torch caching allocator (`Need.torch_slack_usable`, default; an ONNX hint sets it
  `False`) that slack is CREDITED to the device headroom (`usable_device_headroom`); the cache is
  never released by the arbiter. With another process holding X, own models Y allocated and Z
  cached: headroom = `total - X - Y - reserve`. `DeviceAccount` keeps the three parts apart:
  `foreign_bytes` (non-torch minus ledger ONNX bytes), `held_by_pool` `{pool: {owner: bytes}}`,
  `own_slack_bytes`.
- **Slots (small pool's `General.ai_max_loaded_models`).** Only RESIDENT holdings occupy a slot;
  a parallel load of another key is not counted, so the cap can be exceeded by a concurrent load,
  and a race reload registered by `mark_loaded` (S-058) can exceed it by one until the next
  admission reclaims. `mark_loaded` never evicts (it runs under the service lock).
- **Only a measured size may refuse.** A small admission's byte need is `strict` only when its
  footprint is `MEASURED` / `LEARNED` (a clean measurement of this key in this process). An
  unmeasured footprint (a `DECLARED` hint or the `DEFAULT`) is still used to make room — every
  helping offer is executed even if together they cannot cover it — but the load is admitted when
  it still does not fit (INFO log: the size is an estimate). A slot shortfall always refuses.
  `Need.strict` defaults to `True`, so the large pool's needs are unaffected.
- **Unreadable budgets do not refuse small loads.** For a SMALL requester, a device whose budget
  reads all zero (an ONNX EP index torch cannot see, torch without CUDA, a failed or partial-torch
  probe — `MemoryBudgetProbes.device` turns a probe exception into that) or an unknown host
  `MemAvailable` is not byte-checked: only the slot counts, as before the arbiter; one WARNING per
  budget per process. The large pool keeps "unknown = refuse" (its planner's rule).
- **Footprint.** `hint=None` reserves no bytes and is never measured (no probe at all). A hint
  with unknown bytes takes the default 2 GiB device (on a `cuda*` device) / 1 GiB host. Only
  `cuda*` devices are device-accounted; MPS / DirectML / CPU bytes are host-accounted. A load
  window is CLEAN only if no other load, eviction, IN_RUN reservation or lease was active at its
  start and the ledger did not change until `finish_load`. The first clean measurement REPLACES
  the hint or default in both directions; a load of an already learned key only raises it. The
  result is learned per key for this process only (never persisted). Torch device bytes =
  `memory_reserved` delta; ONNX = `mem_get_info` free delta, believed only within [0.5x, 4x] of the
  estimate; host = `RssAnon` delta. A clean lease window re-measures on `release` and only ever
  raises, capped at 4x the first record.
- **Large-pool hooks** (for `../large_models/manager.py`): `begin_use(Pool.LARGE, ...)`,
  `update_parts(pool, key, parts, device=)` after every park / restore / partial drop,
  `reserve(Requester(..., IN_RUN), deficit)` as ladder rung 0, `update_reservation` per phase,
  `release_reservation` at session end, `view().reclaimable(pool, device=, requester=)` for the
  planner's `Reclaimable`. A whole-holding `DROP` with `ok=True` is marked not resident by the
  arbiter; a part offer's `execute` reports its own move.
- A reclaim that raises is logged with context and skipped like an unsuccessful one; it is never
  propagated into the requester.
- `ModelMemoryShortError(RuntimeError)` keeps the pre-arbiter count-cap text verbatim for a slot
  shortfall and names resource, deficit and the busy holders for a byte shortfall.

## Editing map
- New field a policy needs to decide offers: `holdings.HoldingView` / `HoldingsView`, filled in
  `arbiter._view_locked`.
- Reclaim order or prefix choice: `arbiter._offer_sort_key`, `arbiter._choose_prefix`.
- Size defaults, the ONNX bound, the re-measure cap: constants in `footprint.py`.
- What counts as a clean window: `arbiter._open_window_locked` and the epoch bumps.
- The small pool's policy (LRU, count cap): `../model_manager.py`; the large pool's:
  `../large_models/` (implements `arbiter.PoolPolicy`).
