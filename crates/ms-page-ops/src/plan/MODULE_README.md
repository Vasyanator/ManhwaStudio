# Module: crates/ms-page-ops/src/plan

## Purpose
Child modules of `plan.rs`, the PURE planning layer of a structural page
operation: an operation request plus a `ChapterSnapshot` becomes the old -> new
page permutation and a journal-serializable `PageOpPlan` that `fs_exec.rs`
executes as a crash-safe transaction. Nothing here touches the filesystem.
The crate-level contracts (two-tree remap, trash archive, journal phases) are in
the parent `../MODULE_README.md`.

## Architecture
`plan.rs` DECLARES every type of the layer — the page-keyed name helpers and
constants, `PlacementMap`, `StitchGeometry`, `SplitGeometry` +
`SplitTreeRouting`, `CropGeometry` + `CropTreeRouting`, `PageGeometry`, the
compose / new-page / permutation types, the snapshot types, the plan output
types and the `PlanBuilder` accumulator. The children hold the BEHAVIOUR: the
impl blocks and the free functions. Because the children are descendants of
`plan`, they read the types' private fields directly; nothing in a type's
field visibility had to widen.

```text
build_plan (builder.rs)
  -> permutation_for_op (permutation.rs)          request -> old_to_new + new pages
  -> build_{stitch,split,crop}_geometry (geometry.rs)
        -> resolve_split_parts (split.rs) / resolve_crop (crop.rs) / PlacementMap (placement.rs)
  -> plan_src_pages, plan_clean_overlays, plan_layer_pngs, plan_layers_manifest,
     plan_text_info, plan_typing_masks, plan_bubbles (artifacts.rs)
  -> plan_detection (detection.rs)
  -> plan_creates (artifacts.rs)
  -> PageOpPlan
```
The planner order inside `build_plan` is part of the contract (temp-name
numbering and `from`-path dedup in `PlanBuilder` depend on it).

## Files and submodules
- `placement.rs`: `PlacementMap` behaviour (validated construction with or
  without rotation, the per-space mappings `map_uv` / `map_point` /
  `map_extent` / `map_*_rect`, `crop_rect`), `round_to_u32`, and the
  `StitchGeometry` accessors.
- `split.rs`: `SplitTreeRouting` / `SplitGeometry` behaviour (the
  `part_for_*` routing decision by exact overlap area), `resolve_split_parts`,
  `validate_split_routing` (the single legality rule of `order` / `deleted`),
  and the exact-area helpers `polygon_area` / `clip_half_plane` (also used by
  the crop).
- `crop.rs`: `CropTreeRouting` / `CropGeometry` behaviour (the `keeps_*`
  survival tests) and `resolve_crop`, the single crop legality rule.
- `permutation.rs`: `permutation_for_op` and the per-op permutations
  (move, insert / blank, delete, stitch, split, crop).
- `builder.rs`: the `PlanBuilder` methods, `to_pretty` and `build_plan`
  (re-exported by `plan.rs` as `crate::plan::build_plan`).
- `geometry.rs`: snapshot-dependent geometry resolution
  (`build_stitch_geometry`, `build_split_geometry`, `build_crop_geometry`,
  `max_layer_idx_for_page`) and the raster compose planners
  (`plan_crop_raster`, `crop_compose_source`, `plan_split_raster_parts`).
- `artifacts.rs`: per-artifact planners for both trees — source pages, clean
  overlays, layer PNGs, layers manifest, legacy `text_info.json`, typing
  masks, bubbles — and `plan_creates` for inserted / blank pages.
- `detection.rs`: the committed tree's `text_detection/` planner and its
  stitch / split / crop variants.
- `tests.rs`: unit tests of the whole layer (permutations, geometry in every
  coordinate space, name parsers, `build_plan` over in-memory snapshots).

## Contracts and invariants
- Pure: no filesystem access anywhere in this directory; pixel sizes and
  document bodies arrive only through the snapshot.
- Visibility: a function another child calls is `pub(super)` (visible to
  `plan` and its descendants); only what other crate modules use is
  `pub(crate)` and reached through `crate::plan::…` (`build_plan` by
  re-export; types are declared in `plan.rs` itself).
- `PageGeometry` has no per-variant accessor: every planner destructures it
  with its own exhaustive `match` (see the parent readme).
- `PlacementMap` is the single page -> canvas affine; never re-derive it.

## Editing map
- New op kind / index math: `permutation.rs` (plus `PageOpKind` in `lib.rs`).
- Stitch / split / crop geometry rules: `placement.rs`, `split.rs`, `crop.rs`;
  their snapshot-side resolution: `geometry.rs`.
- New page-keyed artifact: a type field in `plan.rs`, a planner in
  `artifacts.rs`, its call in `build_plan` (`builder.rs`).
- Detection files: `detection.rs`.
- New plan action kind: the type in `plan.rs`, a `PlanBuilder` method in
  `builder.rs`, its executor in `../fs_exec.rs`.
