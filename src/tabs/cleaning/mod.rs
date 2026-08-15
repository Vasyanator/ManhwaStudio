mod autoclean;
mod tab;
mod tools;
// GUI-free chapter-level watermark decomposition engine (`I = c + s*B`, solved from several
// occurrences), consumed by the «По главе (точное вычитание)» mode of
// `tools/watermark_removal.rs`.
//
// `allow(dead_code)`: the tool uses the flat-sample path, so the engine's estimated-background
// REFINEMENT surface (`refit_with_refined_backgrounds`, `provisional_background`,
// `SampleBackground::Estimated` and the constants and error variants that belong to it) plus a
// handful of accessors currently have no product caller. They are a finished, deliberately kept
// capability — a chapter with no flat-ring occurrence at all needs them — and every one of them is
// exercised by this module's own tests, which `dead_code` does not count. Removing them would be
// removing measured functionality, not dead weight.
#[allow(dead_code)]
mod watermark_chapter;

pub use tab::{CleaningDrawParams, CleaningTabState};
