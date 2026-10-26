# Module: modules/ai_backend/runtime/large_models

## Purpose
The model-agnostic core of the large-model manager: what a large model weighs block by block,
where each of its bytes may live during each phase of a run (the user's two resource axes
«Видеопамять» x «Оперативная память»), the planner that fits a step pair to the memory budget, and the one forecast arithmetic (device peak,
host peak, seconds) that both the UI and the guard use, the mechanics that move the bytes, and the
manager that admits and drives a run (degrade ladder per axis, learned floors per axis, the large
pool's policy under the residency arbiter). Model packages (FLUX.2 klein, FLUX.3 klein) contribute one
`spec.LargeModelSpec` each; nothing here names a model family.

The memory probes themselves (`MemAvailable`, cgroup v2, `RssAnon + RssShmem`, device
free/total/reserve) are NOT here: they live in the neutral `../memory_budget.py`, shared with the
small-model pool and the residency arbiter above both pools.

## Architecture
Layering (a module imports only from lines above it; stdlib only at import, torch lazily —
in `cost_tables.detect_device_class`, in `inventory`'s `meta` conversion and inside the
mechanics' functions):
```
spec                      (stdlib)
inventory, tiers          (-> spec; inventory -> ..tensor_container, ..safetensors_stream)
cost_tables               (-> ..torch_support, ..memory_probe lazily)
forecast                  (-> tiers, inventory, cost_tables)
planner                   (-> forecast, tiers, ..memory_budget)
ledger                    (-> tiers; stdlib)                       mechanics
loader                    (-> ledger, inventory, ..safetensors_stream, ..memory_budget)
hooks, moves              (-> loader, ledger; hooks -> ..tensor_container; moves -> ..rocm_mmap_transfer in debug mode)
degrade                   (-> planner, moves, tiers, ..memory_budget)              manager layer
manager                   (-> every module above, ..residency, ..memory_budget)
```
Manager data flow of one run (`manager.LargeModelManager`, the large pool's `PoolPolicy` under the
shared `..residency` arbiter): `acquire` takes the lease (`begin_use(LARGE)`, no hint), plans on
fresh probes (own bytes + own torch slack credited, other pools' outstanding reservations claimed,
idle holdings of both pools reclaimable; «Авто» at most one VRAM step above the free-memory fit,
Q-P1) and reserves `plan peak - own bytes` (ADMISSION, which reclaims). The service then drives the
`Session` under its own lock: `load`, `transition`, `run_phase(name, fn)`, `settle`, `finish`. Every step
reconciles each unit from its ledger tier to the plan's tier for that step and updates the run
reservation (remaining peak - own bytes). A memory failure at a step goes to rung 0
(`reserve(IN_RUN, deficit)`), then `degrade.Ladder`.
Mechanics data flow of one run (driven by the manager): `loader.load_component` fills a
component per its `ComponentPlan` (DEVICE units on the device, HOST units in one packed pageable
buffer each, DISK/DROPPED left on `meta`) and registers every unit in the handle's `Ledger`;
`moves.transfer_block_wise` executes each phase transition unit by unit; `hooks.apply_block_residency`
runs a phase whose compute component has HOST/DISK blocks, and is removed before the next move.
`Ledger.holdings()` is what the handle reports up to the residency arbiter.
Data flow of one planning call: the model package resolves its files per weight format ->
`inventory.build_inventories` (checkpoint headers only, safetensors or GGUF through
`..tensor_container`) -> `planner.plan(spec, inventories,
format=, formats=, steps=, limits=, budgets=, shape=, tables=, floor=)`. The planner plans the
STEP GRID of the format lazily: each concrete pair (a cell) expands its `tiers.template_for`
template over the spec's components, tries the template's alternatives in preference order,
solves how many DiT/encoder blocks stay on the device (binary search: below the whole component
the device peak only grows with that number), forecasts each candidate with `forecast.forecast`;
the request (explicit pair, or «Авто» on either axis) resolves to the first fitting cell of its
walk: `Planned(plan, forecast, fit, full_forecast, relaxed, same_as_vram, same_as_ram)` or a
`PlanRefusal` with suggestions. `plan_grid` / `axis_rows` / `plan_axis` give the pickers' rows.

The two axes (`tiers.VramStep`, `tiers.RamStep`; each also `AUTO`, request only):
- «Видеопамять»: `MAX` «Максимум скорости» (everything on the card, kept between runs), `PHASE`
  «Только текущий этап» (the card holds what the current phase needs), `HALF` «Не больше
  половины» (at most half of the usable card; the DiT juggled), `MIN` «Минимум» (no DiT block
  kept on the card: `k = 0`).
- «Оперативная память»: `KEEP` «Держать модели в памяти» (parked parts in RAM, also between
  runs), `RUN` «Только на время задачи» (RAM during the run only), `MIN` «Минимум, читать с
  диска» (nothing parked: juggled blocks read from the file per pass, no CPU encoder).
- The composition table is the `tiers` module header; every step is strict (one idle and one keep
  tier). «Авто» on both axes walks RAM-major (`tiers.STEP_WALK`: give up VRAM before RAM).
- **Host pin** (`LargeModelSpec.host_pin_of(options)` -> component names, e.g. FLUX.2 / FLUX.3
  «Энкодер на CPU»): a request may pin components to host execution. A pinned component ignores
  its role's row of the template: `tiers.host_pinned_policy(ram)` places it — CPU from RAM at EVERY
  VRAM step; between runs in RAM under KEEP, during the run only under RUN, read from its files for
  its own phase and dropped right after under MIN (it is never block-streamed to the CPU). No
  VRAM step's device placement or forecast holds any of it, and neither «Авто» nor a degrade rung
  moves it to the device. The pin is validated by the spec (`host_pin`: a declared component that
  may run on the CPU), is part of the residency key (`LargeModelSpec.key` appends
  `|host_pin=<names>`) and of the learned-floor key (`FloorKey.host_pin`), and is ignored on a
  host run device.
- **Per-axis monotonic invariant, by construction** (`planner` room chain): for the same request
  and budgets a lower VRAM step never forecasts more device bytes than a higher one (full
  forecast), a lower RAM step never more host growth. A cell whose own template fits its room but
  not under its neighbour's peak repeats that neighbour's plan (`same_as_vram` / `same_as_ram`);
  a cell whose own template does not fit its room is refused, never raised on either axis.

## Files and submodules
- `__init__.py`: docstring only; imports nothing (keeps every submodule torch-free to import).
- `spec.py`: what a model package declares — `LargeModelSpec`, `ComponentSpec` (`block_of`,
  `state_keys_of` for 1:N conversions, `device_itemsize`, allowed `ExecDevice`s, block order),
  `PhaseSpec` (`uses`, `compute`, `work(shape, options) -> PhaseWork`), `RequestShape`,
  `FormatInfo`, `Role`; manager-side `LargeModelSpec.formats` / `key_of` (`key()`, with the host
  pin suffix) / `fallbacks` (`Fallback`) / `host_pin_of` (`host_pin()`).
- `inventory.py`: `build_inventory` / `build_inventories` -> `ComponentInventory` (blocks in
  residency order, non-block weights, device and file bytes, largest tensor/block, `fragmented`,
  the load transient `load_host_bytes` from `BlockSpan.convert_bytes`: the spec's conversion run
  on `meta`, outputs that alias the read buffer cost nothing). Files of either container: a
  block-quantized GGUF tensor is a `U8` span (`TensorSpan.quant_type`) sized by its bytes.
- `tiers.py`: `Tier`, `VramStep` / `VRAM_ORDER`, `RamStep` / `RAM_ORDER`, `Steps` (`AUTO_STEPS`,
  `STEP_WALK`), `StepFloor`, `Limits` (hard caps only), the plan types (`ComponentPlan`,
  `PhasePlan`, `Transition`, `ResidencyPlan`, `derive_transitions`) and the templates
  (`Placement` with `resident_cap`, `RolePolicy`, `ProfileTemplate` with `device_share`,
  `template_for`) and `host_pinned_policy` (a pinned component per RAM step).
- `cost_tables.py` + `calibration/<device_class>.json`: the measured cost model (per-phase
  activation and seconds, device constants: runtime overhead, margin, park / restore / per-block
  H2D / disk rates, prefetch depth), `device_class_of`, `load_tables`, `parse_tables`; an entry's
  `measured` flag is per entry (only catch-all entries are forced unmeasured). Tables:
  `rocm_gfx1201` (the reference) and `cuda_sm_120`, both FITTED by
  `tools/large_model_calibration` (`fit`) from real runs, with the fit rules in its `fit.py`.
- `forecast.py`: `forecast(...) -> Forecast` (per phase, per transition, idle; `TimeForecast`;
  `PlanLine`s; `calibrated` / `device_calibrated`, and `PhaseForecast.calibrated` per phase).
- `planner.py`: `Budgets` (incl. `observed_deficit_bytes` of a degrading run and the per-handle
  `device_cap_bytes`), `Reclaimable`,
  `PlanFit` / `FitKind`, `plan`, `plan_grid`, `axis_rows`, `plan_axis`, `guarded_forecast`,
  `Planned`, `PlanRefusal`, `Suggestion` (`format` / `limits` / `steps`), the `REASON_*` codes.
- `ledger.py`: `Ledger` (one tier per `UnitRef`, transient copies via `begin_copy` / `commit` /
  `release` / `move_without_copy`, staging, `holdings()`, `assert_single_copy()`), the debug
  cross-check (`MS_LARGE_MODELS_DEBUG=1`, `arm_cross_check` / `cross_check`).
- `loader.py`: `load_component`, `load_units`, `LoadedComponent` (unit binding `TensorSlot`,
  `host_buffers`), `packed_layout` / `packed_view`, the disk-read progress frame `LoadProgress` /
  `LoadProgressSink` / `component_read_progress`, and the device seam `Transport` /
  `TorchTransport` / `Stream` / `TorchLedgerProbe`.
- `hooks.py`: `apply_block_residency` -> `HookSet` (pre/post forward hooks, device slots, the DISK
  reader thread, `HookStats`), `BlockStreamError`.
- `moves.py`: `transfer_block_wise` -> `TransferReport`, `park`, `restore`, `drop`, `DeviceShort`;
  optional `read_progress` for the DISK / DROPPED units a transfer re-reads.
- `degrade.py`: `classify_failure` -> `RunFailure` (`FailureKind`), `Ladder` (rungs `RungKind`:
  `RECLAIM`, `TIGHTER_DEVICE`, `FALLBACK`, `NEXT_VRAM_STEP`, `NEXT_RAM_STEP`; `Replan`, `Rung`,
  `MAX_RETRIES`), learned floors per axis (`FloorKey` with its `host_pin`, `FloorEntry`,
  `FloorsStore`, `LearnedFloors`).
- `manager.py`: `LargeModelManager` (`register`, `acquire(steps=)`, `profiles(claimed_*_bytes=)` ->
  `ProfilesReport`, `limits` / `set_limits` (partial, atomic: `KEEP_LIMIT`), `floors` /
  `reset_floors`, `status`, `placements` -> `ComponentPlacement`, `health`, `unload` /
  `unload_key`; the `host_device` seam), `Session` (`settle`, `placements`), `PhaseContext`,
  the large `PoolPolicy`, the errors (`LargeModelRefused`, `LargeModelRunFailed`),
  the store seam (`LimitsStore`, `JsonLimitsStore`, `MemoryLimitsStore`, `limits_store_path`) and
  `apply_oom_score_backstop` (Linux `oom_score_adj = 1000`, `General.large_models_oom_score_adj`).
- `_test_fixtures.py`: header-only safetensors writer, the toy three-component spec, round-number
  tables, budget builders (tests only).
- `_mechanics_test_fixtures.py`: the real-torch 6-block toy model, a safetensors writer with data,
  `FakeTransport` (exact byte pools) / `DeferredTransport` (side copies run only at event waits),
  host-budget fakes (tests only).
- `test_ledger.py`, `test_loader.py`, `test_hooks.py`, `test_moves.py`: the mechanics contracts
  below (CPU; torch tests skip without torch).
- `test_degrade.py` (pure, header-only toy plans) and `test_manager.py` (real mechanics on CPU, the
  real arbiter and small pool, fake probes over the fake transports' byte pools): the manager
  contracts below.
- `test_inventory.py` (also the spec validation), `test_tiers.py`, `test_cost_tables.py`,
  `test_forecast.py`, `test_planner.py`: the contracts below. `test_inventory.py` and
  `test_hooks.py` also cover a GGUF file (written with `.._gguf_test_fixtures`; skipped without
  the `gguf` package).
- `test_gguf_residency.py`: GGUF weights through every tier — a toy with a `GgufEmbedding` and 4
  blocks of `GgufLinear` over a real tiny GGUF: loader DEVICE / HOST packed / DISK, hooks streaming
  from the GGUF data start, park / restore, and a manager acquire -> load -> run_phase -> finish
  under RAM KEEP. Output equals the reference exactly, single copy holds, `quant_type` and the
  uint8 weight survive every step (skipped without torch / `gguf`).

## Contracts and invariants
- **No host OOM (the budget's contract, owned by `../memory_budget.py`).** A plan's host growth
  must fit `min(MemAvailable, cgroup room) - reserve` (+ what other pools could reclaim, reported
  as `PlanFit.NEEDS_RECLAIM`); page cache never counts beyond `MemAvailable`; an unknown figure
  refuses (`REASON_BUDGET_UNKNOWN`). The live guard for loads, moves and the hooks' DISK staging
  buffers is `memory_budget.HostWatch` at every block/tensor/buffer boundary.
- **Host run device** (`cpu` / `mps`; every device but `cuda*`, the residency layer's
  `footprint.is_device_accounted` rule, injectable as `LargeModelManager(host_device=)`): planned
  on the HOST budget alone. The forecast folds every device-side byte into the host side (device
  peaks `0`), the planner never reads the device budget, never juggles, and ignores the VRAM axis
  (the RAM step picks the cell: `tiers.host_device_steps`, KEEP -> MAX/KEEP, RUN -> PHASE/RUN,
  MIN -> PHASE/MIN; `profiles` reports `vram.applicable: false`); the manager reports, reserves
  and credits everything on the host side and keeps its floors under `HOST_DEVICE_CLASS`; loads
  and restores count the device-tier bytes against the `HostWatch` (`LoadedComponent.device_is_host`).
- **Never plan device memory above `total - reserve`** (`DeviceBudget.headroom`), and never on an
  unreadable device. The user's `vram_cap` bounds the whole device usage.
- **Single-copy invariant (stated here, enforced by the ledger of the mechanics layer):** a
  block's weights exist in exactly one location (device, host or disk) except the
  `prefetch_depth + 1` blocks in flight. The forecast is written to this invariant: a component
  that moves at a transition is counted in its old place before and its new place after, never
  in both, plus ONE moving unit in flight (on the device side only when a component's device
  bytes change: a host-only move — a CPU component read into or dropped from RAM — has nothing in
  flight on the device).
- **Forecast = max over phases, transitions and the idle state**, computed by ONE function the
  UI and the guard share. **A load's host transient is forecast:** the loader holds one file
  tensor's read buffer plus its non-aliasing conversion outputs at once
  (`ComponentInventory.load_host_bytes`; measured +0.72 GiB FLUX.3 bf16 encoder embedding,
  +2.3 GiB FLUX.2 f16 embedding cast to bf16); every transition that reads a component from
  its files carries it on the host, and the first run's cold boundaries (empty process) fold
  their host peaks into the steady transition of the same name, so the guard and the
  reservation cover the first run too. The device side of a transition stays the steady one.
  The load's `HostWatch` checks the same figure (before the first read, then per tensor) and
  never asks for more than the forecast. It assumes the caching allocator's cache is released at every phase
  boundary (`clear_torch_cache` after each evicted block / at each transition); cache carried
  over from an earlier phase is NOT modelled and is the main source of under-forecasts in the
  seed measurements. Activations are allocated bytes x `reserved_factor` (what the allocator
  reserves inside a phase).
- **The planner never switches the weight format.** Other formats only appear as
  `Suggestion(kind="format")`, computed with the same planner; the user must choose.
- `AUTO` is never a plan value (`ResidencyPlan.steps` is concrete). A learned floor makes each
  axis's effective step at least as economical as that axis's floor;
  `ResidencyPlan.steps_requested` keeps the request. An explicit step is honoured exactly apart
  from the floor; it is never raised on the other axis to fit (refused with `steps` suggestions).
- `Limits` are hard caps over every step (`vram_cap`, `ram_cap`); what stays between runs is the
  axes' decision (the card only under VRAM `MAX`, RAM only under RAM `KEEP`).
- `ResidencyPlan.idle` is the between-runs state and also the assumed state before a run (the
  steady state of repeated runs); before a component's first use it holds at most the more
  economical of its idle and keep tiers.
- **Calibration is judged per phase** (`PhaseForecast.calibrated` = the table is calibrated and
  the phase's cost entry is measured). A family without measured entries on a device class or a
  borrowed table (an unknown device class: the reference table x `GENERIC_SCALE`) makes every
  phase uncalibrated; it is never treated as measured. Only a catch-all entry is forced
  unmeasured; a field wildcard (`decode float32 * *`) keeps its own flag. `Forecast.calibrated` =
  every phase (the UI's "estimate" mark); `Forecast.device_calibrated` = every DEVICE-executed
  phase. **The device activation of an unmeasured device-executed phase chooses the plan but
  never refuses it** (`planner`): every candidate is tried with all activations first; only when
  none fits are those estimated activations taken out (measured phases keep theirs, a CPU phase
  has none on the device), and a device OOM then goes to the ladder. Weight bytes and the whole
  host side stay strict. The `Planned.forecast` is the guarded one (`planner.guarded_forecast`,
  equal to the full forecast when `device_calibrated`; `device_activation` still per phase), so
  reservations, fits and the ladder's comparisons use what the guard enforced.
- **Mechanics: the ledger is the single-copy enforcement.** Every tier change of a unit goes
  through it; a unit holds exactly one tier, a second copy exists only in flight (a move, or a
  hook slot holding the block), at most `prefetch_depth + 1` at once. The forecast's staging
  term (`prefetch_depth + 1` largest remaining blocks, device and — for DISK — host) is exactly
  what `hooks` allocates.
- **Measured transport defaults (micro-benchmarks on ROCm and CUDA; binding):** prefetch depth 1 on a side stream, issued
  only after the current block's compute is queued, event-synchronised (`wait(MAIN, copied)` /
  `wait(SIDE, used)`); parked units in pageable memory, never pinned; DISK staging = two pinned
  buffers of the largest block per hook set, pinned by registering a plain allocation
  (`cudaHostRegister`), never through torch's caching host allocator (it kept freed pinned
  blocks resident); one positional read per contiguous span, one host-to-device copy per block.
- **Parked units are packed:** one pageable buffer per unit with the slot's layout
  (`packed_layout`, 256-byte aligned): one copy per block, and a block buffer (>= 32 MiB) is its
  own mapping, so freeing it returns the RAM (per-tensor allocations stay in glibc's arenas).
- **Stored layout = the conversion's output, derived on `meta`** (`ComponentSpec.convert` must be
  device-agnostic torch code; the DISK tier applies it to device views on every pass).
- **Moves:** device peak <= max(start, end) + one unit on RESERVED bytes; `empty_cache` after every
  evicted unit and at the end of a transfer (the forecast's phase-boundary assumption); a
  `HostMemoryShort` before parking switches the remaining outgoing units to DROPPED.
- **ROCm:** nothing here copies a file-mapped (`rw-p`) tensor to the GPU (reads land in anonymous
  or registered buffers); whole-module moves elsewhere go through
  `..rocm_mmap_transfer.move_module_to`.
- Calibration files are tracked package data; a missing or malformed reference file is an
  installation defect (`OSError` / `ValueError`), never a silent fallback.

- **Manager: one owner of where a large model's bytes live.** Every tier change of a run goes
  through `Session._reconcile` (ledger tier -> plan tier per unit: block-wise moves first evicting,
  then parks interleaved with admissions, then fresh loads; cache released after). A unit planned
  in RAM that holds no bytes (dropped by a host shortfall) stays dropped unless its component is
  needed by the step; a needed one, an exec-device change or a format change re-reads the
  component whole, so a phase function fetches modules from its `PhaseContext` on every call and
  must be restartable from its inputs.
- **Safe points** are step boundaries only (a load, a transition, a whole phase, `settle`). A
  failure there: rung 0 `reserve(IN_RUN, deficit)` (counts only if other holdings were actually
  reclaimed: idle small models and other large handles go before any degrade, R2), then the
  ladder; at most `MAX_RETRIES` (3) retries per run. Every degrade rung is a RE-PLAN through
  `planner.plan` (the forecast stays the guard) and walks ONLY the axis of the resource that
  failed: a device failure (OOM, `DeviceShort`, sysmem spill) tries a tighter device room
  (juggling plans), the spec's `fallbacks` of the failing phase, then the next VRAM steps with the
  same RAM step (the accumulated observed deficit taken out of each step's room); a host failure
  tries the next RAM steps with the same VRAM step. A tighter / next-step plan is taken only if
  it lowers the failing step's forecast peak. A run that ends by a memory failure drops everything.
- **The format is never changed by the manager**; a refusal carries the planner's format
  suggestions and names the shortest resource. The user's steps are never rewritten; a learned
  floor (device class, model, format, host pin) is recorded PER AXIS only after a successful run that ended
  lower on that axis, each axis only lowered, applies to the next plans (`planner` floor), and is
  reset explicitly. Floors of the old single profile scale in the store are skipped (one INFO);
  an entry stored without `host_pin` is an unpinned one.
- **Per-handle device ceiling** (`acquire(..., device_cap_bytes=)`, `profiles(...,
  device_cap_bytes=)`, `Budgets.device_cap_bytes`): bounds the device peak this handle's plan may
  forecast, after claims and reclaim, for the admission and every degrade re-plan; it never raises
  the user's `vram_cap` and does not change what HALF halves. An explicit step that cannot fit
  under it is refused (device), never lowered. A multi-handle service uses it to keep one handle
  at most what a higher step of the same operation would give it.
- **`finish` only shrinks.** It parks or drops toward `plan.idle` and never loads a component or
  moves a unit toward the device (a unit whose between-runs tier is more resident than where it
  is stays there; an absent component stays absent): an early failure costs no disk read.
  `Session.settle()` is the explicit reconcile to `plan.idle` with loads (a safe point).
- **Disk-read progress has one session-level sink.** `Session.set_load_progress(sink)` receives
  every checkpoint read of a step as `loader.LoadProgress` frames (component, file being read,
  the component's files, done / total file bytes of THIS read, `reread`): a fresh load (`load`,
  a transition, a phase entry, the reload after a degrade retry, `settle`) is one stream over the
  whole component; the DISK / DROPPED units a step brings back are one stream per component and
  step (`reread=True`, aggregated in `moves` and throttled there once). Frames come synchronously
  on the session's thread, at most every 0.2 s per stream (`safetensors_stream`) plus one 100%
  frame. Hook streaming inside a phase and `finish` report nothing. An explicit
  `Session.load(progress=)` replaces the sink for that call's fresh loads.
- **Placements** (`ComponentPlacement`, `LargeModelManager.placements(key)`, `status()`
  `components`) are read from the handle's ledger only: no probe, never waits for a session.
- **Run reservation.** A session holds one reservation = remaining forecast peak - bytes the
  handle already holds (+ rung-0 extras), updated after every step and released by `finish`, so a
  concurrent small admission cannot take memory a running phase was promised.
- **Large pool policy.** Offers per idle component: PARK_TO_HOST (class A; the arbiter takes it
  only while the host still fits) and DROP (class C, reload = file bytes / cold disk rate).
  `execute` only TRIES the handle's run lock (a running session is never waited for, and is never
  offered anyway, R1) and reports the new parts (`update_parts`; `mark_unloaded` when nothing is
  left). A large lease's owner defaults to its key: `execute` never takes a service lock.
- **Locks and threads.** `acquire` runs before the service lock (the admission may wait for other
  services' evictions), unless the service (i) holds no small-pool lease and registers no reclaim
  callback that takes its lock, and (ii) opens several handles in one operation: it then acquires
  every session under its lock, in one fixed handle order (FLUX.2: pipeline, then encoder; a run
  lock is taken blocking, so acquiring outside the lock would invert the order with a concurrent
  single-handle action). FLUX.3 (one handle) acquires before its lock. A handle's run lock
  serializes its sessions (taken after the lease, released before it); reclaim and `unload` only
  try it; the manager's `_lock` is a leaf over its dicts only. A session is driven by one thread.
- **Sysmem spill** (NVIDIA Windows only, `memory_budget.detect_sysmem_spill`): checked after every
  phase; a spilled phase keeps its result and degrades the remaining phases
  (`Session.hints` gets `nvidia_sysmem_fallback`).
- Limits and floors persist through `LimitsStore`; the stage-1 `JsonLimitsStore` lives at
  `limits_store_path(<backend cache root>)`, the cache root being resolved by the composition root
  through `engines.paddle_onnx.resolve_compiled_cache_root()` (no upward import from `runtime/`).

## Editing map
- To change what a step pair means, edit `tiers.template_for` and its table (and `test_tiers.py`
  `CompositionTests`, `test_planner.py`); a pinned component's placement per RAM step:
  `tiers.host_pinned_policy` (`test_planner.py` `HostPinTests`, `test_manager.py` `HostPinTests`).
- To change how a fit is judged, how alternatives are tried, the room chain or the Auto walk, edit
  `planner.py`; `GridInvariantTests` is the property test of the monotonic invariant.
- To change the forecast arithmetic (staging, margin, transition accounting, time model), edit
  `forecast.py`; keep it the only place that computes a peak.
- To recalibrate a device, measure with `tools/large_model_calibration` (`flux3` / `flux2`), write
  `calibration/<device_class>.json` with its `fit` (the class's current table as `--base`), and
  judge a validation run with its `check` (see its `MODULE_README.md`; `parse_tables` documents
  the schema). A new device class is a new file.
- To support a new model, write its `LargeModelSpec` in the model package; nothing here changes.
- To change how memory is probed or the reserves, edit `../memory_budget.py`, not this package.
- To change the admission (planner inputs, Q-P1, the reservation), the session steps (incl.
  `finish` / `settle`), the load-progress sink, the placements or the pool offers, edit `manager.py`; the rung order, what
  counts as a memory failure or the floors, `degrade.py`; a model-specific rung is a
  `spec.Fallback` in the model package.
- To change how a component is loaded per tier, edit `loader.py`; how a juggled block reaches the
  device during a forward (slots, prefetch, DISK reads), `hooks.py`; the phase-transition order,
  `moves.py`; tier bookkeeping or what is reported up, `ledger.py`.
