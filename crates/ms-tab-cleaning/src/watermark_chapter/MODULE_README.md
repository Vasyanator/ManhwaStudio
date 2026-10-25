# Module: crates/ms-tab-cleaning/src/watermark_chapter

## Purpose
The GUI-free chapter-level watermark decomposition engine: exact, AI-free removal of a
semi-transparent publisher mark by SOLVING its compositing equation `I = c + s*B` from occurrences
observed over different backgrounds, then removing by division `B = (I - c)/s`. The design and
the measurements its constants rest on are in `dev-docs/watermark_chapter_decomposition_plan.md`;
the pipeline, the key types and the footprint contract are stated in the header of `mod.rs`.

Consumer: the «По главе (точное вычитание)» mode of `../tools/watermark_removal/`, plus
`../tools/watermark_entry.rs` (library intake) and `../tools/watermark_library_window.rs`
(wording). Job orchestration, the region editor, `CanvasView` patches, i18n and persistence are
NOT this module's business.

## Architecture
One directory module. `mod.rs` holds every TYPE of the engine (so private fields stay visible to
every child without widening), the shared tuning constants, the error types, `PixelRect` and the
numeric / validation helpers (`validate_page`, `validate_rect`, `luma_of`, `count_f32`,
`ratio_u64`). The children hold behaviour only, one pipeline stage each; private helper structs
used by exactly one stage (`FitPlan`, `FittedPlanes`, `GainVerdict`, `PaddedPatch`) live with that
stage.

Pipeline (per `WatermarkKind`):
`samples.rs` (validate a calibration sample) -> `fit.rs` (estimate the `c`/`s` model) ->
`detection.rs` (discover anchors, find occurrences, gain test) -> `removal.rs` (divide out the
mark). `kinds.rs` ties a mark's samples, template and model together; `model.rs` answers what a
fitted model and its conditioning mean; `template.rs` owns the correlation reference and the
footprint measurement.

## Files and submodules
- `mod.rs`: header (pipeline, key types, footprint contract, entry invariant), constants, error
  types, `PixelRect` + helpers, all type declarations, child `mod`s and the re-exports the rest of
  the crate uses (`crate::watermark_chapter::X`).
- `template.rs`: `AlphaBlend` operator, `alpha_blend_operator` / `operator_from_id`,
  `impl MarkTemplate` (construction, anchor columns, NCC reference), column clustering, and the
  footprint measurement (`trimmed_footprint_from_model`, `trimmed_footprint_from_template`,
  `mark_extent_from_template`).
- `samples.rs`: `SampleBackground`, `CalibrationSample`, `RingCoverage`, `SampleVerdict`,
  `SampleParams` behaviour; `validate_calibration_sample`, `calibration_sample_from_page`.
- `model.rs`: `AlphaUncertainty`, `ModelConditioning`, `WatermarkModel`, `MarkSignature`,
  `SubpixelShift` behaviour.
- `fit.rs`: `estimate_model` and its plan / regressions / deposit-exact planes / robust line fits.
- `kinds.rs`: `Occurrence`, `DetectionParams`, `WatermarkKind` behaviour; `find_matching_kind`,
  `refit_with_refined_backgrounds`.
- `detection.rs`: anchor discovery, `find_occurrences`, `scan_page` / `scan_chapter`, overlap
  resolution, NCC, gain test, Lucas-Kanade shift, `provisional_background`.
- `removal.rs`: `RemovalResidual` behaviour, `remove_occurrence`, `remove_occurrences_on_page`.
- `tests.rs`: the engine's unit tests (synthetic marks; no files written).

## Contracts and invariants
- GUI-free and `Send`: no egui, no backend, no user-visible strings; `Display` of errors is
  developer diagnostics and the tool layer maps variants to i18n keys.
- Every rect is validated inside the page before flat indexing (entry invariant in `mod.rs`).
- Visibility: an item the rest of the crate names is `pub(crate)` in its child and re-exported by
  `mod.rs` (`pub(super) use child::{...}`), so `crate::watermark_chapter::X` paths never name a
  child. A helper shared only between children is `pub(super)`. Re-exports used only by tests
  elsewhere in the crate are `#[cfg(test)]`.
- `lib.rs` declares the module with `allow(dead_code)` for the deliberately kept
  estimated-background refinement surface (see the comment there); it covers every child.

## Editing map
- Change a threshold: the constants block in `mod.rs` (each carries its rationale; change only
  against the measurements in the plan).
- Add or change a type: `mod.rs`; its behaviour goes into the stage file that owns it.
- Sample acceptance / ring measurement: `samples.rs`. Model fit: `fit.rs`. Conditioning wording
  inputs and model accessors: `model.rs`. Detection and anchors: `detection.rs`. Removal maths:
  `removal.rs`. Footprints and the template: `template.rs`. Kind matching / refit: `kinds.rs`.
