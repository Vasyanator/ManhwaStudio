/*
File: crates/ms-text-render/src/test_fixtures.rs

Purpose:
Test-only owner of the repository-tracked "caller font" fixture the unit tests of this crate
load: one path, so a fresh clone runs every test and a move of the file is one edit.

Key functions:
- caller_font_path(): Arimo Regular, tracked as the bundled typesetting font
  `fonts/простой V2.ttf`.

Notes:
- Why Arimo and not the `fonts/ui` stack: the registry, pool and degrade tests need a font
  that is NOT part of the bundled base database and has no other face of its family (an
  upright-only caller font). Every `fonts/ui` family is in the base, and Noto Sans even
  ships its Bold there. Arimo also keeps the tests' recorded geometry: it is the metric
  source of Liberation Sans 2.x, the untracked fixture these tests were written against.
- The characterization matrices use the `fonts/ui` stack on purpose (`characterization.rs`).
*/

use std::path::PathBuf;

/// Path of the tracked caller-font fixture (Arimo Regular: Latin, Cyrillic, Greek, Hebrew;
/// GPOS pair kerning such as `AV`; one upright face, family outside the bundled base).
///
/// Resolved from `CARGO_MANIFEST_DIR` (this crate sits two levels below the repository
/// root). Tests read it and fail loudly when it is missing: it is tracked, so absence
/// means a broken checkout, not an optional fixture.
pub(crate) fn caller_font_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fonts/простой V2.ttf")
}
