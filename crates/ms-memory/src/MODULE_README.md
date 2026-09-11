# Module: crates/ms-memory/src

## Purpose
The image-cache memory POLICY of the application, and only the policy: which profile the
user picked, how much of the machine is actually free, how much pressure that means, and
which cached resources should be dropped first. It never owns a pixel, a
`egui::TextureHandle`, or any tab state — cache owners keep their storage and ask this
crate what to evict.

Single-file crate (`lib.rs`). `src/main.rs` mounts it with
`pub use ms_memory as memory_manager;`, so application call sites keep writing
`crate::memory_manager::…`.

## Architecture
Three layers, all pure except the middle one:

1. Profile — `MemoryProfile` (persisted in `user_config.json`) and `MemoryManager`, a small
   `RwLock`-backed handle so the profile can be hot-applied from the settings UI while
   workers read it.
2. Probe — `current_memory_availability()` reads what the OS reports free
   (`/proc/meminfo` on Linux, `vm_stat` on macOS, `None` elsewhere). This is the crate's
   only side effect and its only failure mode.
3. Policy — `classify_memory_pressure()` turns availability + profile into a
   `MemoryPressure`, `MemoryBudget` derives per-profile limits, and
   `select_eviction_candidates()` orders `CacheResourceInfo` entries into a
   `CacheEvictionReport`. All pure functions with unit tests.

## Files and submodules
- `lib.rs`: everything above. Edit the profile enum for a new user-facing profile, the
  probe for a new OS, the thresholds for a new pressure rule, and the selector for a new
  eviction ordering.

## Contracts and invariants
- Position in the dependency order: this crate sits BELOW `config` — `crates/ms-config`
  imports `MemoryProfile` from it. It must never grow a dependency in the other direction.
- The probe returns `Option`: "the OS did not tell us" is a distinct answer from "there is
  no memory free", and callers must treat `None` as unknown, not as pressure.
- Eviction selection only ever proposes RECONSTRUCTABLE resources (`CacheReloadCost`), and
  never a page inside the pinned window (`pinned_page_window`). A cache owner is free to
  ignore the report, but must not evict something the report did not name.
- `MemoryProfile` is serialized into `user_config.json`, so its serde representation is a
  persisted format: renaming a variant breaks existing user configs.
- Crate boundary: dependencies are `ms-log`, `ms-i18n` and `serde`. No egui, no image
  crate, no application layers — that is what keeps it a leaf that type-checks in parallel
  with the binary.

## Editing map
- To add or rename a memory profile, see `MemoryProfile` (and remember it is persisted).
- To support a new OS memory probe, see `current_memory_availability`.
- To change when the app considers itself under pressure, see `PressureThresholds` /
  `classify_memory_pressure`.
- To change what gets dropped first, see `select_eviction_candidates` — and add the case to
  its tests, which are the only guard on the ordering.
