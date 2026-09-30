# Module: crates/ms-actions

## Purpose
Pure, GUI-free generic undo/redo engine for ManhwaStudio (design notes:
`dev-docs/unified_action_system.md`). It provides only the generic mechanism — a
self-inverting command contract, an in-memory history and an optional raster delta
primitive — and no domain behavior. Concrete Ops live with their domain and must NOT
live here: `BubbleSnapshotOp` (`ms-canvas/src/bubble_action.rs`), `CleanOverlayDiffOp`
(`ms-models/src/clean_overlays_model.rs`) and `PsEditOp` (`ms-tab-ps-editor/src/edit_op.rs`).

## Architecture
Two small, independent pieces:

- `ReversibleAction` (`action.rs`): the Koharu-style command contract. A command
  instance is one-shot: `apply` mutates the caller's context AND captures the
  previous state it overwrote into `self`, so `inverse()` can build the reverse
  command purely from local data without re-reading the context.
- `ActionHistory<A>` (`history.rs`): a generic in-memory undo/redo engine holding
  two stacks (`undo` VecDeque with oldest at the front, `redo` Vec), a count
  `limit`, and an optional `weight_budget` (total-bytes cap). It drives
  `apply`/`inverse` and enforces redo-branch truncation and undo eviction under
  both caps. There is no durable history log.
- `RasterDiff` (`raster_diff.rs`, behind the optional `raster` feature): a pure,
  domain-agnostic, reversible RGBA8 delta primitive. It TILES a
  straight-alpha RGBA8 image into `tile_side`-sized tiles and stores only changed
  tiles, each with its own tight bbox plus a zstd-compressed signed `[i16;4]`
  delta. It is a standalone primitive: it does NOT implement `ReversibleAction`;
  domain Ops wrap it (`CleanOverlayDiffOp`, `PsEditOp`).

## Files and submodules
- `src/lib.rs`: crate root; crate-level clippy lints; re-exports `ReversibleAction`,
  `ActionHistory` and (feature `raster`) the `raster_diff` types.
- `src/action.rs`: the `ReversibleAction` trait and its lifecycle contract,
  including the defaulted `weight()` (retained bytes, default 0) for memory
  budgeting.
- `src/history.rs`: `ActionHistory<A>` (`new`, `with_weight_budget`,
  `set_weight_budget`, `apply`, `record`, `undo`, `redo`, `can_undo`/`can_redo`,
  `undo_len`/`redo_len`, `limit`, `undo_weight`, `weight_budget`, `peek_*_label`,
  `clear`).
- `src/raster_diff.rs` (feature `raster`): `RasterDiff`, `RasterTileDiff`,
  `DirtyRect`, `ApplyDirection`, `RasterDiffError`. `from_rgba` builds a diff from
  full-image before/after buffers; `from_region_pixels` builds one from
  region-local before/after buffers (tile origins stored in image coordinates so
  it `apply`s against the full image identically to `from_rgba`); `apply`
  reapplies it Forward (redo) or Reverse (undo).
- `tests/history.rs`: contract tests using an in-test `SetOp` over `Vec<i32>`.
- `tests/raster_diff.rs` (feature `raster`): round-trip/reversibility, tight-bbox,
  multi-tile, non-multiple sizes, clamping, and error-path tests.

## Contracts and invariants
- Pure `std` by default: no egui/eframe/tokio/image, no dependency on the app
  crate or any domain type. Dependency purity is enforced by the manifest (the way
  Koharu keeps `koharu-core` HTTP-free).
- The `raster` feature is the ONLY thing that pulls an extra dependency (`zstd`,
  0.13, matching the workspace root). The `raster_diff` module and its deps are
  fully gated, so `cargo check -p ms-actions` (no features) stays std-only.
- `RasterDiff` contract: operates on raw straight-alpha RGBA8 `&[u8]` of exactly
  `width*height*4` bytes; buffer length, image size, tile side, and payload
  integrity are validated up front and reported as `RasterDiffError` — the public
  API never panics on bad input. Index/length math uses checked/`try_from`
  conversions (no lossy `as`); the hot pixel loop clamps each channel to `[0,255]`.
  Reversibility: for any before/after, `apply(after, Reverse) == before` and
  `apply(before, Forward) == after`. Tiling uses `div_ceil(tile_side)` and clamps
  boundary tiles, so image dimensions need not be a multiple of `tile_side`.
- `from_region_pixels` contract: `before`/`after` are region-local straight-RGBA8
  of `region_size` area; the region must fit within `image_size` (else
  `TileOutOfBounds`). It tiles the REGION and stores each changed sub-tile's
  origin in IMAGE coordinates (`region_origin` + local offset), reusing the same
  tight-bbox + LE `i16` delta + zstd payload as `from_rgba`. The payload is
  independent of the origin offset, so a region-built tile is byte-identical to
  the equivalent full-image tile and `apply`s against a full-image buffer
  identically. Use it to skip a full-page scan when only a small region changed.
- Weight budget: `ActionHistory` optionally bounds total undo-stack bytes via
  `ReversibleAction::weight()` (default 0). `undo_weight` is tracked
  incrementally (added on push, subtracted on eviction/undo; zeroed on `clear`),
  never recomputed. Eviction pops oldest (front) while `undo_len > limit` OR
  (`weight_budget` set AND `undo_weight > budget` AND `undo_len > 1`). The
  `undo_len > 1` guard means a single entry larger than the whole budget is still
  retained — there is always at least one undoable step. The budget applies to
  the UNDO stack only; redo weight is not budgeted (redo is truncated on every
  fresh edit).
- Self-inverting contract: for a given action instance, `apply` runs exactly once
  before `inverse()`; `apply` captures `prev`; `inverse()` is pure and must not
  touch the context.
- `apply` truncates the redo branch (a fresh edit abandons any redoable future)
  and evicts the oldest undo entries (front) beyond `limit`.
- `record` is the observer-style entry point: the caller already mutated the
  domain directly, so `record` performs the same redo-truncation and `limit`
  eviction as `apply` but skips the forward `apply` call. The recorded action
  must arrive with its captured state populated so a later `undo` is valid.
- `undo` applies the original action's inverse and moves the original to the redo
  stack; `redo` re-applies the original and pushes it back to undo. On `redo`,
  `apply` re-captures `prev` from the current (post-undo) context — this equals the
  originally captured `prev` because undo restored the pre-apply state; it is
  intentional (mirrors Koharu).
- `limit == 0` means run-but-forget: the action still runs, nothing is retained.
- No panics on the public API; `apply` failures propagate as `A::Err`. The engine
  does no I/O and holds no locks; heavy work belongs inside the action and is
  scheduled off the GUI thread by the caller.

## Editing map
- To change the command contract (capture/inverse semantics), see `action.rs`.
- To change undo/redo stack behavior, the cap, or add history introspection, see
  `history.rs`.
- To change raster delta tiling, serialization, compression, or apply/clamp
  behavior, see `raster_diff.rs`. Keep it feature-gated and dependency-free apart
  from `zstd`.
- Concrete domain Ops belong in their domain crate, not here.
